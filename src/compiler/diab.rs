// Copyright 2018 Mozilla Foundation
// Copyright 2018 Felix Obenhuber <felix@obenhuber.de>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::compiler::args::{
    ArgDisposition, ArgInfo, ArgToStringResult, ArgsIter, Argument, FromArg, IntoArg,
    NormalizedDisposition, PathTransformerFn, SearchableArgInfo,
};
use crate::compiler::c::{CCompilerImpl, CCompilerKind, Language, ParsedArguments};
use crate::compiler::{Cacheable, ColorMode, CompileCommand, CompilerArguments};
use crate::dist;
use crate::errors::*;
use crate::mock_command::{CommandCreatorSync, RunCommand};
use crate::util::{run_input_output, OsStrExt};
use log::Level::Trace;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;

#[derive(Clone, Debug)]
pub struct Diab;

impl CCompilerImpl for Diab {
    fn kind(&self) -> CCompilerKind {
        CCompilerKind::Diab
    }
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
    ) -> CompilerArguments<ParsedArguments> {
        parse_arguments(arguments, cwd, &ARGS[..])
    }

    fn preprocess<T>(
        &self,
        creator: &T,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        may_dist: bool,
    ) -> SFuture<process::Output>
    where
        T: CommandCreatorSync,
    {
        preprocess(creator, executable, parsed_args, cwd, env_vars, may_dist)
    }

    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> Result<(CompileCommand, Option<dist::CompileCommand>, Cacheable)> {
        generate_compile_commands(path_transformer, executable, parsed_args, cwd, env_vars)
    }
}

ArgData! { pub
    DoCompilation,
    Output(PathBuf),
    PassThrough(OsString),
    PreprocessorArgumentFlag,
    PreprocessorArgument(OsString),
    PreprocessorArgumentPath(PathBuf),
    TooHardFlag,
    TooHard(OsString),
}

use self::ArgData::*;

counted_array!(pub static ARGS: [ArgInfo<ArgData>; _] = [
    flag!("-", TooHardFlag),
    flag!("-##", TooHardFlag),
    flag!("-###", TooHardFlag),
    take_arg!("-@", OsString, Concatenated, TooHard),
    take_arg!("-D", OsString, CanBeSeparated, PreprocessorArgument),
    flag!("-E", TooHardFlag),
    take_arg!("-I", PathBuf, CanBeSeparated, PreprocessorArgumentPath),
    take_arg!("-L", OsString, Separated, PassThrough),
    flag!("-P", TooHardFlag),
    flag!("-S", TooHardFlag),
    take_arg!("-U", OsString, CanBeSeparated, PreprocessorArgument),
    flag!("-V", TooHardFlag),
    flag!("-VV", TooHardFlag),
    take_arg!("-W", OsString, Separated, PassThrough),
    flag!("-Xmake-dependency", PreprocessorArgumentFlag),
    flag!(
        "-Xmake-dependency-canonicalize-path-off",
        PreprocessorArgumentFlag
    ),
    take_arg!(
        "-Xmake-dependency-savefile",
        PathBuf,
        Concatenated('='),
        PreprocessorArgumentPath
    ),
    take_arg!(
        "-Xmake-dependency-target",
        OsString,
        Concatenated('='),
        PreprocessorArgument
    ),
    flag!("-c", DoCompilation),
    take_arg!(
        "-include",
        PathBuf,
        CanBeSeparated,
        PreprocessorArgumentPath
    ),
    take_arg!("-l", OsString, Separated, PassThrough),
    take_arg!("-o", PathBuf, Separated, Output),
    take_arg!("-t", OsString, Separated, PassThrough),
]);

/// Parse `arguments`, determining whether it is supported.
///
/// If any of the entries in `arguments` result in a compilation that
/// cannot be cached, return `CompilerArguments::CannotCache`.
/// If the commandline described by `arguments` is not compilation,
/// return `CompilerArguments::NotCompilation`.
/// Otherwise, return `CompilerArguments::Ok(ParsedArguments)`, with
/// the `ParsedArguments` struct containing information parsed from
/// `arguments`.
pub fn parse_arguments<S>(
    arguments: &[OsString],
    cwd: &Path,
    arg_info: S,
) -> CompilerArguments<ParsedArguments>
where
    S: SearchableArgInfo<ArgData>,
{
    let mut common_args = vec![];
    let mut compilation = false;
    let mut input_arg = None;
    let mut multiple_input = false;
    let mut output_arg = None;
    let mut preprocessor_args = vec![];

    // Custom iterator to expand `@` arguments which stand for reading a file
    // and interpreting it as a list of more arguments.
    let it = ExpandAtArgs::new(cwd, arguments);

    for arg in ArgsIter::new(it, arg_info) {
        let arg = try_or_cannot_cache!(arg, "argument parse");
        // Check if the value part of this argument begins with '@'. If so, we either
        // failed to expand it, or it was a concatenated argument - either way, bail.
        // We refuse to cache concatenated arguments (like "-include@foo") because they're a
        // mess. See https://github.com/mozilla/sccache/issues/150#issuecomment-318586953
        match arg {
            Argument::WithValue(_, ref v, ArgDisposition::Separated)
            | Argument::WithValue(_, ref v, ArgDisposition::CanBeConcatenated(_))
            | Argument::WithValue(_, ref v, ArgDisposition::CanBeSeparated(_)) => {
                if v.clone().into_arg_os_string().starts_with("@") {
                    cannot_cache!("@");
                }
            }
            // Empirically, concatenated arguments appear not to interpret '@' as
            // an include directive, so just continue.
            Argument::WithValue(_, _, ArgDisposition::Concatenated(_))
            | Argument::Raw(_)
            | Argument::UnknownFlag(_)
            | Argument::Flag(_, _) => {}
        }

        match arg.get_data() {
            Some(TooHardFlag) | Some(TooHard(_)) => {
                cannot_cache!(arg.flag_str().expect("Can't be Argument::Raw/UnknownFlag",))
            }
            Some(DoCompilation) => compilation = true,
            Some(Output(p)) => output_arg = Some(p.clone()),
            Some(PreprocessorArgument(_))
            | Some(PreprocessorArgumentFlag)
            | Some(PreprocessorArgumentPath(_))
            | Some(PassThrough(_)) => {}
            None => match arg {
                Argument::Raw(ref val) => {
                    if input_arg.is_some() {
                        multiple_input = true;
                    }
                    input_arg = Some(val.clone());
                }
                Argument::UnknownFlag(_) => {}
                _ => unreachable!(),
            },
        }
        let args = match arg.get_data() {
            Some(PassThrough(_)) => &mut common_args,
            Some(PreprocessorArgumentFlag)
            | Some(PreprocessorArgument(_))
            | Some(PreprocessorArgumentPath(_)) => &mut preprocessor_args,
            Some(DoCompilation) | Some(Output(_)) => continue,
            Some(TooHardFlag) | Some(TooHard(_)) => unreachable!(),
            None => match arg {
                Argument::Raw(_) => continue,
                Argument::UnknownFlag(_) => &mut common_args,
                _ => unreachable!(),
            },
        };
        // Normalize attributes such as "-I foo", "-D FOO=bar", as
        // "-Ifoo", "-DFOO=bar", etc. and "-includefoo", "idirafterbar" as
        // "-include foo", "-idirafter bar", etc.
        let norm = match arg.flag_str() {
            Some(s) if s.len() == 2 => NormalizedDisposition::Concatenated,
            _ => NormalizedDisposition::Separated,
        };
        args.extend(arg.normalize(norm).iter_os_strings());
    }

    // We only support compilation.
    if !compilation {
        return CompilerArguments::NotCompilation;
    }
    // Can't cache compilations with multiple inputs.
    if multiple_input {
        cannot_cache!("multiple input files");
    }
    let input = match input_arg {
        Some(i) => i.to_owned(),
        // We can't cache compilation without an input.
        None => cannot_cache!("no input file"),
    };
    let language = match Language::from_file_name(Path::new(&input)) {
        Some(l) => l,
        None => cannot_cache!("unknown source language"),
    };

    let output = output_arg
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&input).with_extension("o"));

    let mut outputs = HashMap::new();
    outputs.insert("obj", output);

    CompilerArguments::Ok(ParsedArguments {
        input: input.into(),
        language,
        depfile: None,
        outputs,
        preprocessor_args,
        common_args,
        extra_hash_files: vec![],
        msvc_show_includes: false,
        profile_generate: false,
        // FIXME: Implement me.
        color_mode: ColorMode::Auto,
    })
}

pub fn preprocess<T>(
    creator: &T,
    executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
    _may_dist: bool,
) -> SFuture<process::Output>
where
    T: CommandCreatorSync,
{
    let mut cmd = creator.clone().new_command_sync(&executable);
    cmd.arg("-E")
        .arg(&parsed_args.input)
        .args(&parsed_args.preprocessor_args)
        .args(&parsed_args.common_args)
        .env_clear()
        .envs(env_vars.iter().map(|&(ref k, ref v)| (k, v)))
        .current_dir(cwd);

    if log_enabled!(Trace) {
        trace!("preprocess: {:?}", cmd);
    }
    run_input_output(cmd, None)
}

pub fn generate_compile_commands(
    _path_transformer: &mut dist::PathTransformer,
    executable: &Path,
    parsed_args: &ParsedArguments,
    cwd: &Path,
    env_vars: &[(OsString, OsString)],
) -> Result<(CompileCommand, Option<dist::CompileCommand>, Cacheable)> {
    trace!("compile");

    let out_file = match parsed_args.outputs.get("obj") {
        Some(obj) => obj,
        None => return Err("Missing object file output".into()),
    };

    let mut arguments: Vec<OsString> = vec![
        "-c".into(),
        parsed_args.input.clone().into(),
        "-o".into(),
        out_file.into(),
    ];
    arguments.extend(parsed_args.preprocessor_args.clone());
    arguments.extend(parsed_args.common_args.clone());
    let command = CompileCommand {
        executable: executable.to_owned(),
        arguments,
        env_vars: env_vars.to_owned(),
        cwd: cwd.to_owned(),
    };

    Ok((command, None, Cacheable::Yes))
}

pub struct ExpandAtArgs<'a> {
    cwd: &'a Path,
    stack: Vec<OsString>,
}

impl<'a> ExpandAtArgs<'a> {
    pub fn new(cwd: &'a Path, args: &[OsString]) -> Self {
        ExpandAtArgs {
            stack: args.iter().rev().map(|a| a.to_owned()).collect(),
            cwd,
        }
    }
}

impl<'a> Iterator for ExpandAtArgs<'a> {
    type Item = OsString;

    fn next(&mut self) -> Option<OsString> {
        loop {
            let arg = self.stack.pop()?;

            // Just return non @ arguments
            if !arg.starts_with("-@") {
                return Some(arg);
            }

            let value = match arg.split_prefix("-@") {
                Some(arg) => arg,
                None => return Some(arg),
            };

            // Return options that produce additional output and are not cacheable
            if value.starts_with("E") || value.starts_with("O") || value.starts_with("@") {
                return Some(arg);
            }

            // According to diab [1], @file means:
            // Read command line options from either a file or an environment
            // variable. When -@name is encountered on the command line, the
            // driver first looks for an environment variable with the given
            // name and substitutes its value. If an environment variable is
            // not found then the driver tries to open a file with given name
            // and substitutes the contents of the file. If neither an
            // environment variable or a file can be found, an error message
            // is issued and the driver terminates.
            //
            // [1]: http://www.vxdev.com/docs/vx55man/diab5.0ppc/c-invoke.htm#3000619
            //
            // The environment variable feature is *not* supported by sccache
            // since this would raise the need for the clients environment
            // and not just env::var. This is technically possible, but
            // considered as a unneeded edge case for now.

            let mut contents = String::new();
            let file = self.cwd.join(&value);
            let res = File::open(&file).and_then(|mut f| f.read_to_string(&mut contents));
            if res.is_err() {
                // Failed to read the file, so return the argumet as it is.
                // This will result in a CannotCache.
                return Some(arg);
            }
            if contents.contains('"') || contents.contains('\'') {
                return Some(arg);
            }
            let new_args = contents.split_whitespace().collect::<Vec<_>>();
            self.stack.extend(new_args.iter().rev().map(|s| s.into()));
        }
    }
}

#[cfg(test)]
mod test {
    use super::{
        dist, generate_compile_commands, parse_arguments, Language, OsString, ParsedArguments,
        PathBuf, ARGS,
    };
    use crate::compiler::*;
    use crate::mock_command::*;
    use crate::test::utils::*;
    use futures::Future;
    use std::fs::File;
    use std::io::Write;
    use tempdir::TempDir;

    fn _parse_arguments(arguments: &[String]) -> CompilerArguments<ParsedArguments> {
        let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
        parse_arguments(&args, ".".as_ref(), &ARGS[..])
    }

    #[test]
    fn test_parse_arguments_simple() {
        let args = stringvec!["-c", "foo.c", "-o", "foo.o"];
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!(Language::C, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert!(common_args.is_empty());
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_default_name() {
        let args = stringvec!["-c", "foo.c"];
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!(Language::C, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert!(common_args.is_empty());
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_extra() {
        let args = stringvec!["-c", "foo.cc", "-fabc", "-o", "foo.o", "-mxyz"];
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.cc"), input.to_str());
        assert_eq!(Language::Cxx, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert_eq!(ovec!["-fabc", "-mxyz"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_values() {
        let args = stringvec![
            "-c", "foo.cxx", "-fabc", "-I", "include", "-o", "foo.o", "-include", "file"
        ];
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.cxx"), input.to_str());
        assert_eq!(Language::Cxx, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert_eq!(ovec!["-Iinclude", "-include", "file"], preprocessor_args);
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_preprocessor_args() {
        let args = stringvec![
            "-c",
            "foo.c",
            "-fabc",
            "-Xmake-dependency",
            "-Xmake-dependency-canonicalize-path-off",
            "-Xmake-dependency-savefile=bar",
            "-Xmake-dependency-target=foo",
            "-o",
            "foo.o"
        ];
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&args) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!(Language::C, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert_eq!(
            ovec![
                "-Xmake-dependency",
                "-Xmake-dependency-canonicalize-path-off",
                "-Xmake-dependency-savefile=bar",
                "-Xmake-dependency-target=foo"
            ],
            preprocessor_args
        );
        assert_eq!(ovec!["-fabc"], common_args);
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_parse_arguments_empty_args() {
        assert_eq!(CompilerArguments::NotCompilation, _parse_arguments(&vec![]));
    }

    #[test]
    fn test_parse_arguments_not_compile() {
        assert_eq!(
            CompilerArguments::NotCompilation,
            _parse_arguments(&stringvec!["-o", "foo"])
        );
    }

    #[test]
    fn test_parse_arguments_too_many_inputs() {
        assert_eq!(
            CompilerArguments::CannotCache("multiple input files", None),
            _parse_arguments(&stringvec!["-c", "foo.c", "-o", "foo.o", "bar.c"])
        );
    }

    #[test]
    fn test_parse_arguments_link() {
        assert_eq!(
            CompilerArguments::NotCompilation,
            _parse_arguments(&stringvec!["-shared", "foo.o", "-o", "foo.so", "bar.o"])
        );
    }

    #[test]
    fn test_parse_dry_run() {
        assert_eq!(
            CompilerArguments::CannotCache("-##", None),
            _parse_arguments(&stringvec!["-##", "-c", "foo.c"])
        );

        assert_eq!(
            CompilerArguments::CannotCache("-###", None),
            _parse_arguments(&stringvec!["-###", "-c", "foo.c"])
        );
    }

    #[test]
    fn test_at_signs() {
        let cannot_cache = CompilerArguments::CannotCache("-@", None);
        assert_eq!(_parse_arguments(&["-@@foo".into()]), cannot_cache);
        assert_eq!(_parse_arguments(&["-@E=foo".into()]), cannot_cache);
        assert_eq!(_parse_arguments(&["-@E+foo".into()]), cannot_cache);
        assert_eq!(_parse_arguments(&["-@O=foo".into()]), cannot_cache);
        assert_eq!(_parse_arguments(&["-@O+foo".into()]), cannot_cache);
    }

    #[test]
    fn test_at_signs_file_not_readable() {
        let td = TempDir::new("sccache").unwrap();
        let arg = format!("-@{}", td.path().join("foo").display());
        // File foo doesn't exist.
        assert_eq!(
            _parse_arguments(&[arg]),
            CompilerArguments::CannotCache("-@", None)
        );
    }

    #[test]
    fn test_at_signs_file() {
        let td = TempDir::new("sccache").unwrap();
        File::create(td.path().join("foo"))
            .unwrap()
            .write_all(b"-c foo.c -o foo.o")
            .unwrap();
        let arg = format!("-@{}", td.path().join("foo").display());
        let ParsedArguments {
            input,
            language,
            depfile: _,
            outputs,
            preprocessor_args,
            msvc_show_includes,
            common_args,
            ..
        } = match _parse_arguments(&[arg]) {
            CompilerArguments::Ok(args) => args,
            o @ _ => panic!("Got unexpected parse result: {:?}", o),
        };
        assert!(true, "Parsed ok");
        assert_eq!(Some("foo.c"), input.to_str());
        assert_eq!(Language::C, language);
        assert_map_contains!(outputs, ("obj", PathBuf::from("foo.o")));
        assert_eq!(1, outputs.len());
        assert!(preprocessor_args.is_empty());
        assert!(common_args.is_empty());
        assert!(!msvc_show_includes);
    }

    #[test]
    fn test_compile_simple() {
        let creator = new_creator();
        let f = TestFixture::new();
        let parsed_args = ParsedArguments {
            input: "foo.c".into(),
            language: Language::C,
            depfile: None,
            outputs: vec![("obj", "foo.o".into())].into_iter().collect(),
            preprocessor_args: vec![],
            common_args: vec![],
            extra_hash_files: vec![],
            msvc_show_includes: false,
            profile_generate: false,
            color_mode: ColorMode::Auto,
        };
        let compiler = &f.bins[0];
        // Compiler invocation.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        let mut path_transformer = dist::PathTransformer::new();
        let (command, _, cacheable) = generate_compile_commands(
            &mut path_transformer,
            &compiler,
            &parsed_args,
            f.tempdir.path(),
            &[],
        )
        .unwrap();
        let _ = command.execute(&creator).wait();
        assert_eq!(Cacheable::Yes, cacheable);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }
}
