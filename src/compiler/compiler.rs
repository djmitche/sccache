// Copyright 2016 Mozilla Foundation
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

use crate::cache::{Cache, CacheWrite, Storage};
use crate::compiler::c::{CCompiler, CCompilerKind};
use crate::compiler::clang::Clang;
use crate::compiler::diab::Diab;
use crate::compiler::gcc::GCC;
use crate::compiler::msvc;
use crate::compiler::msvc::MSVC;
use crate::compiler::rust::Rust;
use crate::dist;
#[cfg(feature = "dist-client")]
use crate::dist::pkg;
use futures::Future;
use futures_cpupool::CpuPool;
use crate::mock_command::{exit_status, CommandChild, CommandCreatorSync, RunCommand};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
#[cfg(any(feature = "dist-client", unix))]
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempdir::TempDir;
use tempfile::NamedTempFile;
use tokio_timer::Timeout;
use crate::util::{fmt_duration_as_secs, ref_env, run_input_output};

use crate::errors::*;

#[derive(Clone, Debug)]
pub struct CompileCommand {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub env_vars: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

impl CompileCommand {
    pub fn execute<T>(self, creator: &T) -> SFuture<process::Output>
    where
        T: CommandCreatorSync,
    {
        let mut cmd = creator.clone().new_command_sync(self.executable);
        cmd.args(&self.arguments)
            .env_clear()
            .envs(self.env_vars)
            .current_dir(self.cwd);
        Box::new(run_input_output(cmd, None))
    }
}

/// Supported compilers.
#[derive(Debug, PartialEq, Clone)]
pub enum CompilerKind {
    /// A C compiler.
    C(CCompilerKind),
    /// A Rust compiler.
    Rust,
}

impl CompilerKind {
    pub fn lang_kind(&self) -> String {
        match self {
            CompilerKind::C(_) => "C/C++",
            CompilerKind::Rust => "Rust",
        }.to_string()
    }
}

/// An interface to a compiler for argument parsing.
pub trait Compiler<T>: Send + 'static
where
    T: CommandCreatorSync,
{
    /// Return the kind of compiler.
    fn kind(&self) -> CompilerKind;
    /// Retrieve a packager
    #[cfg(feature = "dist-client")]
    fn get_toolchain_packager(&self) -> Box<dyn pkg::ToolchainPackager>;
    /// Determine whether `arguments` are supported by this compiler.
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
    ) -> CompilerArguments<Box<dyn CompilerHasher<T> + 'static>>;
    fn box_clone(&self) -> Box<dyn Compiler<T>>;
}

impl<T: CommandCreatorSync> Clone for Box<dyn Compiler<T>> {
    fn clone(&self) -> Box<dyn Compiler<T>> {
        self.box_clone()
    }
}

/// An interface to a compiler for hash key generation, the result of
/// argument parsing.
pub trait CompilerHasher<T>: fmt::Debug + Send + 'static
where
    T: CommandCreatorSync,
{
    /// Given information about a compiler command, generate a hash key
    /// that can be used for cache lookups, as well as any additional
    /// information that can be reused for compilation if necessary.
    fn generate_hash_key(
        self: Box<Self>,
        creator: &T,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        may_dist: bool,
        pool: &CpuPool,
    ) -> SFuture<HashResult>;

    /// Return the state of any `--color` option passed to the compiler.
    fn color_mode(&self) -> ColorMode;

    /// Look up a cached compile result in `storage`. If not found, run the
    /// compile and store the result.
    fn get_cached_or_compile(
        self: Box<Self>,
        dist_client: Result<Option<Arc<dyn dist::Client>>>,
        creator: T,
        storage: Arc<dyn Storage>,
        arguments: Vec<OsString>,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        cache_control: CacheControl,
        pool: CpuPool,
    ) -> SFuture<(CompileResult, process::Output)> {
        let out_pretty = self.output_pretty().into_owned();
        debug!("[{}]: get_cached_or_compile: {:?}", out_pretty, arguments);
        let start = Instant::now();
        let may_dist = match dist_client {
            Ok(Some(_)) => true,
            _ => false
        };
        let result = self.generate_hash_key(
            &creator,
            cwd.clone(),
            env_vars,
            may_dist,
            &pool,
        );
        Box::new(result.then(move |res| -> SFuture<_> {
            debug!(
                "[{}]: generate_hash_key took {}",
                out_pretty,
                fmt_duration_as_secs(&start.elapsed())
            );
            let (key, compilation, weak_toolchain_key) = match res {
                Err(Error(ErrorKind::ProcessError(output), _)) => {
                    return f_ok((CompileResult::Error, output));
                }
                Err(e) => return f_err(e),
                Ok(HashResult {
                    key,
                    compilation,
                    weak_toolchain_key,
                }) => (key, compilation, weak_toolchain_key),
            };
            trace!("[{}]: Hash key: {}", out_pretty, key);
            // If `ForceRecache` is enabled, we won't check the cache.
            let start = Instant::now();
            let cache_status = if cache_control == CacheControl::ForceRecache {
                f_ok(Cache::Recache)
            } else {
                storage.get(&key)
            };

            // Set a maximum time limit for the cache to respond before we forge
            // ahead ourselves with a compilation.
            let timeout = Duration::new(60, 0);
            let cache_status = Timeout::new(cache_status, timeout);

            // Check the result of the cache lookup.
            Box::new(cache_status.then(move |result| {
                let duration = start.elapsed();
                let outputs = compilation
                    .outputs()
                    .map(|(key, path)| (key.to_string(), cwd.join(path)))
                    .collect::<HashMap<_, _>>();

                let miss_type = match result {
                    Ok(Cache::Hit(mut entry)) => {
                        debug!(
                            "[{}]: Cache hit in {}",
                            out_pretty,
                            fmt_duration_as_secs(&duration)
                        );
                        let mut stdout = Vec::new();
                        let mut stderr = Vec::new();
                        drop(entry.get_object("stdout", &mut stdout));
                        drop(entry.get_object("stderr", &mut stderr));
                        let write = pool.spawn_fn(move || {
                            for (key, path) in &outputs {
                                let dir = match path.parent() {
                                    Some(d) => d,
                                    None => bail!("Output file without a parent directory!"),
                                };
                                // Write the cache entry to a tempfile and then atomically
                                // move it to its final location so that other rustc invocations
                                // happening in parallel don't see a partially-written file.
                                let mut tmp = NamedTempFile::new_in(dir)?;
                                let mode = entry.get_object(&key, &mut tmp)?;
                                tmp.persist(path)?;
                                if let Some(mode) = mode {
                                    set_file_mode(&path, mode)?;
                                }
                            }
                            Ok(())
                        });
                        let output = process::Output {
                            status: exit_status(0),
                            stdout: stdout,
                            stderr: stderr,
                        };
                        let result = CompileResult::CacheHit(duration);
                        return Box::new(write.map(|_| (result, output))) as SFuture<_>;
                    }
                    Ok(Cache::Miss) => {
                        debug!(
                            "[{}]: Cache miss in {}",
                            out_pretty,
                            fmt_duration_as_secs(&duration)
                        );
                        MissType::Normal
                    }
                    Ok(Cache::Recache) => {
                        debug!(
                            "[{}]: Cache recache in {}",
                            out_pretty,
                            fmt_duration_as_secs(&duration)
                        );
                        MissType::ForcedRecache
                    }
                    Err(err) => {
                        if err.is_elapsed() {
                            debug!(
                                "[{}]: Cache timed out {}",
                                out_pretty,
                                fmt_duration_as_secs(&duration)
                            );
                            MissType::TimedOut
                        } else {
                            error!("[{}]: Cache read error: {}", out_pretty, err);
                            if err.is_inner() {
                                let err = err.into_inner().unwrap();
                                for e in err.iter().skip(1) {
                                    error!("[{}] \t{}", out_pretty, e);
                                }
                            }
                            MissType::CacheReadError
                        }
                    }
                };

                // Cache miss, so compile it.
                let start = Instant::now();
                let compile = dist_or_local_compile(
                    dist_client,
                    creator,
                    cwd,
                    compilation,
                    weak_toolchain_key,
                    out_pretty.clone(),
                );

                Box::new(
                    compile.and_then(move |(cacheable, dist_type, compiler_result)| {
                        let duration = start.elapsed();
                        if !compiler_result.status.success() {
                            debug!(
                                "[{}]: Compiled but failed, not storing in cache",
                                out_pretty
                            );
                            return f_ok((CompileResult::CompileFailed, compiler_result))
                                as SFuture<_>;
                        }
                        if cacheable != Cacheable::Yes {
                            // Not cacheable
                            debug!("[{}]: Compiled but not cacheable", out_pretty);
                            return f_ok((CompileResult::NotCacheable, compiler_result));
                        }
                        debug!(
                            "[{}]: Compiled in {}, storing in cache",
                            out_pretty,
                            fmt_duration_as_secs(&duration)
                        );
                        let write = pool.spawn_fn(move || -> Result<_> {
                            let mut entry = CacheWrite::new();
                            for (key, path) in &outputs {
                                let mut f = File::open(&path)?;
                                let mode = get_file_mode(&f)?;
                                entry.put_object(key, &mut f, mode).chain_err(|| {
                                    format!("failed to put object `{:?}` in zip", path)
                                })?;
                            }
                            Ok(entry)
                        });
                        let write = write.chain_err(|| "failed to zip up compiler outputs");
                        let o = out_pretty.clone();
                        Box::new(
                            write
                                .and_then(move |mut entry| {
                                    if !compiler_result.stdout.is_empty() {
                                        let mut stdout = &compiler_result.stdout[..];
                                        entry.put_object("stdout", &mut stdout, None)?;
                                    }
                                    if !compiler_result.stderr.is_empty() {
                                        let mut stderr = &compiler_result.stderr[..];
                                        entry.put_object("stderr", &mut stderr, None)?;
                                    }

                                    // Try to finish storing the newly-written cache
                                    // entry. We'll get the result back elsewhere.
                                    let future = storage.put(&key, entry).then(move |res| {
                                        match res {
                                            Ok(_) => debug!(
                                                "[{}]: Stored in cache successfully!",
                                                out_pretty
                                            ),
                                            Err(ref e) => debug!(
                                                "[{}]: Cache write error: {:?}",
                                                out_pretty, e
                                            ),
                                        }
                                        res.map(|duration| CacheWriteInfo {
                                            object_file_pretty: out_pretty,
                                            duration: duration,
                                        })
                                    });
                                    let future = Box::new(future);
                                    Ok((
                                        CompileResult::CacheMiss(
                                            miss_type, dist_type, duration, future,
                                        ),
                                        compiler_result,
                                    ))
                                }).chain_err(move || format!("failed to store `{}` to cache", o)),
                        )
                    }),
                )
            }))
        }))
    }

    /// A descriptive string about the file that we're going to be producing.
    ///
    /// This is primarily intended for debug logging and such, not for actual
    /// artifact generation.
    fn output_pretty(&self) -> Cow<'_, str>;

    fn box_clone(&self) -> Box<dyn CompilerHasher<T>>;
}

#[cfg(not(feature = "dist-client"))]
fn dist_or_local_compile<T>(
    _dist_client: Result<Option<Arc<dyn dist::Client>>>,
    creator: T,
    _cwd: PathBuf,
    compilation: Box<dyn Compilation>,
    _weak_toolchain_key: String,
    out_pretty: String,
) -> SFuture<(Cacheable, DistType, process::Output)>
where
    T: CommandCreatorSync,
{
    let mut path_transformer = dist::PathTransformer::new();
    let compile_commands = compilation
        .generate_compile_commands(&mut path_transformer)
        .chain_err(|| "Failed to generate compile commands");
    let (compile_cmd, _dist_compile_cmd, cacheable) = match compile_commands {
        Ok(cmds) => cmds,
        Err(e) => return f_err(e),
    };

    debug!("[{}]: Compiling locally", out_pretty);
    Box::new(
        compile_cmd
            .execute(&creator)
            .map(move |o| (cacheable, DistType::NoDist, o)),
    )
}

#[cfg(feature = "dist-client")]
fn dist_or_local_compile<T>(
    dist_client: Result<Option<Arc<dyn dist::Client>>>,
    creator: T,
    cwd: PathBuf,
    compilation: Box<dyn Compilation>,
    weak_toolchain_key: String,
    out_pretty: String,
) -> SFuture<(Cacheable, DistType, process::Output)>
where
    T: CommandCreatorSync,
{
    use futures::future;
    use std::io;

    let mut path_transformer = dist::PathTransformer::new();
    let compile_commands = compilation
        .generate_compile_commands(&mut path_transformer)
        .chain_err(|| "Failed to generate compile commands");
    let (compile_cmd, dist_compile_cmd, cacheable) = match compile_commands {
        Ok(cmds) => cmds,
        Err(e) => return f_err(e),
    };

    let dist_client = match dist_client {
        Ok(Some(dc)) => dc,
        Ok(None) => {
            debug!("[{}]: Compiling locally", out_pretty);
            return Box::new(
                compile_cmd
                    .execute(&creator)
                    .map(move |o| (cacheable, DistType::NoDist, o)),
            );
        },
        Err(e) => {
            return f_err(e);
        }
    };

    debug!("[{}]: Attempting distributed compilation", out_pretty);
    let compile_out_pretty = out_pretty.clone();
    let compile_out_pretty2 = out_pretty.clone();
    let compile_out_pretty3 = out_pretty.clone();
    let compile_out_pretty4 = out_pretty.clone();
    let local_executable = compile_cmd.executable.clone();
    // TODO: the number of map_errs is subideal, but there's no futures-based carrier trait AFAIK
    Box::new(future::result(dist_compile_cmd.ok_or_else(|| "Could not create distributed compile command".into()))
        .and_then(move |dist_compile_cmd| {
            debug!("[{}]: Creating distributed compile request", compile_out_pretty);
            let dist_output_paths = compilation.outputs()
                .map(|(_key, path)| path_transformer.to_dist_abs(&cwd.join(path)))
                .collect::<Option<_>>()
                .ok_or_else(|| Error::from("Failed to adapt an output path for distributed compile"))?;
            compilation.into_dist_packagers(path_transformer)
                .map(|packagers| (dist_compile_cmd, packagers, dist_output_paths))
        })
        .and_then(move |(mut dist_compile_cmd, (inputs_packager, toolchain_packager, outputs_rewriter), dist_output_paths)| {
            debug!("[{}]: Identifying dist toolchain for {:?}", compile_out_pretty2, local_executable);
            dist_client.put_toolchain(&local_executable, &weak_toolchain_key, toolchain_packager)
                .and_then(|(dist_toolchain, maybe_dist_compile_executable)| {
                    if let Some(dist_compile_executable) = maybe_dist_compile_executable {
                        dist_compile_cmd.executable = dist_compile_executable;
                    }
                    Ok((dist_client, dist_compile_cmd, dist_toolchain, inputs_packager, outputs_rewriter, dist_output_paths))
                })
        })
        .and_then(move |(dist_client, dist_compile_cmd, dist_toolchain, inputs_packager, outputs_rewriter, dist_output_paths)| {
            debug!("[{}]: Requesting allocation", compile_out_pretty3);
            dist_client.do_alloc_job(dist_toolchain.clone())
                .and_then(move |jares| {
                    let alloc = match jares {
                        dist::AllocJobResult::Success { job_alloc, need_toolchain: true } => {
                            debug!("[{}]: Sending toolchain {} for job {}",
                                compile_out_pretty3, dist_toolchain.archive_id, job_alloc.job_id);
                            Box::new(dist_client.do_submit_toolchain(job_alloc.clone(), dist_toolchain)
                                .and_then(move |res| {
                                    match res {
                                        dist::SubmitToolchainResult::Success => Ok(job_alloc),
                                        dist::SubmitToolchainResult::JobNotFound =>
                                            bail!("Job {} not found on server", job_alloc.job_id),
                                        dist::SubmitToolchainResult::CannotCache =>
                                            bail!("Toolchain for job {} could not be cached by server", job_alloc.job_id),
                                    }
                                })
                                .chain_err(|| "Could not submit toolchain"))
                        },
                        dist::AllocJobResult::Success { job_alloc, need_toolchain: false } =>
                            f_ok(job_alloc),
                        dist::AllocJobResult::Fail { msg } =>
                            f_err(Error::from("Failed to allocate job").chain_err(|| msg)),
                    };
                    alloc
                        .and_then(move |job_alloc| {
                            let job_id = job_alloc.job_id;
                            debug!("[{}]: Running job", compile_out_pretty3);
                            dist_client.do_run_job(job_alloc, dist_compile_cmd, dist_output_paths, inputs_packager)
                                .map(move |res| (job_id, res))
                                .chain_err(|| "could not run distributed compilation job")
                        })
                })
                .and_then(move |(job_id, (jres, path_transformer))| {
                    let jc = match jres {
                        dist::RunJobResult::Complete(jc) => jc,
                        dist::RunJobResult::JobNotFound => bail!("Job {} not found on server", job_id),
                    };
                    info!("fetched {:?}", jc.outputs.iter().map(|&(ref p, ref bs)| (p, bs.lens().to_string())).collect::<Vec<_>>());
                    let mut output_paths: Vec<PathBuf> = vec![];
                    macro_rules! try_or_cleanup {
                        ($v:expr) => {{
                            match $v {
                                Ok(v) => v,
                                Err(e) => {
                                    // Do our best to clear up. We may end up deleting a file that we just wrote over
                                    // the top of, but it's better to clear up too much than too little
                                    for local_path in output_paths.iter() {
                                        if let Err(e) = fs::remove_file(local_path) {
                                            if e.kind() != io::ErrorKind::NotFound {
                                                warn!("{} while attempting to clear up {}", e, local_path.display())
                                            }
                                        }
                                    }
                                    return Err(e)
                                },
                            }
                        }};
                    }

                    for (path, output_data) in jc.outputs {
                        let len = output_data.lens().actual;
                        let local_path = try_or_cleanup!(path_transformer.to_local(&path)
                            .chain_err(|| format!("unable to transform output path {}", path)));
                        output_paths.push(local_path);
                        // Do this first so cleanup works correctly
                        let local_path = output_paths.last().expect("nothing in vec after push");

                        let mut file = try_or_cleanup!(File::create(&local_path)
                            .chain_err(|| format!("Failed to create output file {}", local_path.display())));
                        let count = try_or_cleanup!(io::copy(&mut output_data.into_reader(), &mut file)
                            .chain_err(|| format!("Failed to write output to {}", local_path.display())));

                        assert!(count == len);
                    }
                    try_or_cleanup!(outputs_rewriter.handle_outputs(&path_transformer, &output_paths)
                        .chain_err(|| "failed to rewrite outputs from compile"));
                    Ok((DistType::Ok, jc.output.into()))
                })
        })
        .or_else(move |e| {
            let mut errmsg = e.to_string();
            for cause in e.iter() {
                errmsg.push_str(": ");
                errmsg.push_str(&cause.to_string());
            }
            // Client errors are considered fatal.
            match e {
                Error(ErrorKind::HttpClientError(_), _) => f_err(e),
                _ => {
                    warn!("[{}]: Could not perform distributed compile, falling back to local: {}", compile_out_pretty4, errmsg);
                    Box::new(compile_cmd.execute(&creator).map(|o| (DistType::Error, o)))
                }
            }
        })
        .map(move |(dt, o)| (cacheable, dt, o))
    )
}

impl<T: CommandCreatorSync> Clone for Box<dyn CompilerHasher<T>> {
    fn clone(&self) -> Box<dyn CompilerHasher<T>> {
        self.box_clone()
    }
}

/// An interface to a compiler for actually invoking compilation.
pub trait Compilation {
    /// Given information about a compiler command, generate a command that can
    /// execute the compiler.
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
    ) -> Result<(CompileCommand, Option<dist::CompileCommand>, Cacheable)>;

    /// Create a function that will create the inputs used to perform a distributed compilation
    #[cfg(feature = "dist-client")]
    fn into_dist_packagers(
        self: Box<Self>,
        _path_transformer: dist::PathTransformer,
    ) -> Result<(
        Box<dyn pkg::InputsPackager>,
        Box<dyn pkg::ToolchainPackager>,
        Box<dyn OutputsRewriter>,
    )> {
        bail!("distributed compilation not implemented")
    }

    /// Returns an iterator over the results of this compilation.
    ///
    /// Each item is a descriptive (and unique) name of the output paired with
    /// the path where it'll show up.
    fn outputs<'a>(&'a self) -> Box<dyn Iterator<Item = (&'a str, &'a Path)> + 'a>;
}

#[cfg(feature = "dist-client")]
pub trait OutputsRewriter {
    /// Perform any post-compilation handling of outputs, given a Vec of the dist_path and local_path
    fn handle_outputs(
        self: Box<Self>,
        path_transformer: &dist::PathTransformer,
        output_paths: &[PathBuf],
    ) -> Result<()>;
}

#[cfg(feature = "dist-client")]
pub struct NoopOutputsRewriter;
#[cfg(feature = "dist-client")]
impl OutputsRewriter for NoopOutputsRewriter {
    fn handle_outputs(
        self: Box<Self>,
        _path_transformer: &dist::PathTransformer,
        _output_paths: &[PathBuf],
    ) -> Result<()> {
        Ok(())
    }
}

/// Result of generating a hash from a compiler command.
pub struct HashResult {
    /// The hash key of the inputs.
    pub key: String,
    /// An object to use for the actual compilation, if necessary.
    pub compilation: Box<dyn Compilation + 'static>,
    /// A weak key that may be used to identify the toolchain
    pub weak_toolchain_key: String,
}

/// Possible results of parsing compiler arguments.
#[derive(Debug, PartialEq)]
pub enum CompilerArguments<T> {
    /// Commandline can be handled.
    Ok(T),
    /// Cannot cache this compilation.
    CannotCache(&'static str, Option<String>),
    /// This commandline is not a compile.
    NotCompilation,
}

macro_rules! cannot_cache {
    ($why:expr) => {
        return CompilerArguments::CannotCache($why, None);
    };
    ($why:expr, $extra_info:expr) => {
        return CompilerArguments::CannotCache($why, Some($extra_info));
    };
}

macro_rules! try_or_cannot_cache {
    ($arg:expr, $why:expr) => {{
        match $arg {
            Ok(arg) => arg,
            Err(e) => cannot_cache!($why, e.to_string()),
        }
    }};
}

/// Specifics about distributed compilation.
#[derive(Debug, PartialEq)]
pub enum DistType {
    /// Distribution was not enabled.
    NoDist,
    /// Distributed compile success.
    Ok,
    /// Distributed compile failed.
    Error,
}

/// Specifics about cache misses.
#[derive(Debug, PartialEq)]
pub enum MissType {
    /// The compilation was not found in the cache, nothing more.
    Normal,
    /// Cache lookup was overridden, recompilation was forced.
    ForcedRecache,
    /// Cache took too long to respond.
    TimedOut,
    /// Error reading from cache
    CacheReadError,
}

/// Information about a successful cache write.
pub struct CacheWriteInfo {
    pub object_file_pretty: String,
    pub duration: Duration,
}

/// The result of a compilation or cache retrieval.
pub enum CompileResult {
    /// An error made the compilation not possible.
    Error,
    /// Result was found in cache.
    CacheHit(Duration),
    /// Result was not found in cache.
    ///
    /// The `CacheWriteFuture` will resolve when the result is finished
    /// being stored in the cache.
    CacheMiss(MissType, DistType, Duration, SFuture<CacheWriteInfo>),
    /// Not in cache, but the compilation result was determined to be not cacheable.
    NotCacheable,
    /// Not in cache, but compilation failed.
    CompileFailed,
}

/// The state of `--color` options passed to a compiler.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ColorMode {
    Off,
    On,
    Auto,
}

impl Default for ColorMode {
    fn default() -> ColorMode {
        ColorMode::Auto
    }
}

/// Can't derive(Debug) because of `CacheWriteFuture`.
impl fmt::Debug for CompileResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            &CompileResult::Error => write!(f, "CompileResult::Error"),
            &CompileResult::CacheHit(ref d) => write!(f, "CompileResult::CacheHit({:?})", d),
            &CompileResult::CacheMiss(ref m, ref dt, ref d, _) => {
                write!(f, "CompileResult::CacheMiss({:?}, {:?}, {:?}, _)", d, m, dt)
            }
            &CompileResult::NotCacheable => write!(f, "CompileResult::NotCacheable"),
            &CompileResult::CompileFailed => write!(f, "CompileResult::CompileFailed"),
        }
    }
}

/// Can't use derive(PartialEq) because of the `CacheWriteFuture`.
impl PartialEq<CompileResult> for CompileResult {
    fn eq(&self, other: &CompileResult) -> bool {
        match (self, other) {
            (&CompileResult::Error, &CompileResult::Error) => true,
            (&CompileResult::CacheHit(_), &CompileResult::CacheHit(_)) => true,
            (
                &CompileResult::CacheMiss(ref m, ref dt, _, _),
                &CompileResult::CacheMiss(ref n, ref dt2, _, _),
            ) => m == n && dt == dt2,
            (&CompileResult::NotCacheable, &CompileResult::NotCacheable) => true,
            (&CompileResult::CompileFailed, &CompileResult::CompileFailed) => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
fn get_file_mode(file: &File) -> Result<Option<u32>> {
    use std::os::unix::fs::MetadataExt;
    Ok(Some(file.metadata()?.mode()))
}

#[cfg(windows)]
fn get_file_mode(_file: &File) -> Result<Option<u32>> {
    Ok(None)
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    let p = Permissions::from_mode(mode);
    fs::set_permissions(path, p)?;
    Ok(())
}

#[cfg(windows)]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Can this result be stored in cache?
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Cacheable {
    Yes,
    No,
}

/// Control of caching behavior.
#[derive(Debug, PartialEq)]
pub enum CacheControl {
    /// Default caching behavior.
    Default,
    /// Ignore existing cache entries, force recompilation.
    ForceRecache,
}

/// Creates a future that will write `contents` to `path` inside of a temporary
/// directory.
///
/// The future will resolve to the temporary directory and an absolute path
/// inside that temporary directory with a file that has the same filename as
/// `path` contains the `contents` specified.
///
/// Note that when the `TempDir` is dropped it will delete all of its contents
/// including the path returned.
pub fn write_temp_file(
    pool: &CpuPool,
    path: &Path,
    contents: Vec<u8>,
) -> SFuture<(TempDir, PathBuf)> {
    let path = path.to_owned();
    pool.spawn_fn(move || -> Result<_> {
        let dir = TempDir::new("sccache")?;
        let src = dir.path().join(path);
        let mut file = File::create(&src)?;
        file.write_all(&contents)?;
        Ok((dir, src))
    }).chain_err(|| "failed to write temporary file")
}

/// If `executable` is a known compiler, return `Some(Box<Compiler>)`.
fn detect_compiler<T>(
    creator: &T,
    executable: &Path,
    env: &[(OsString, OsString)],
    pool: &CpuPool,
) -> SFuture<Option<Box<dyn Compiler<T>>>>
where
    T: CommandCreatorSync,
{
    trace!("detect_compiler: {}", executable.display());

    // First, see if this looks like rustc.
    let filename = match executable.file_stem() {
        None => return f_err("could not determine compiler kind"),
        Some(f) => f,
    };
    let rustc_vv = if filename.to_string_lossy().to_lowercase() == "rustc" {
        // Sanity check that it's really rustc.
        let executable = executable.to_path_buf();
        let child = creator
            .clone()
            .new_command_sync(&executable)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear()
            .envs(ref_env(env))
            .args(&["-vV"])
            .spawn();
        let output = child.and_then(move |child| {
            child
                .wait_with_output()
                .chain_err(|| "failed to read child output")
        });
        Box::new(output.map(|output| {
            if output.status.success() {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    if stdout.starts_with("rustc ") {
                        return Some(stdout);
                    }
                }
            }
            None
        }))
    } else {
        f_ok(None)
    };

    let creator = creator.clone();
    let executable = executable.to_owned();
    let env = env.to_owned();
    let pool = pool.clone();
    Box::new(rustc_vv.and_then(move |rustc_vv| {
        if let Some(rustc_verbose_version) = rustc_vv {
            debug!("Found rustc");
            Box::new(
                Rust::new(creator, executable, &env, &rustc_verbose_version, pool)
                    .map(|c| Some(Box::new(c) as Box<dyn Compiler<T>>)),
            )
        } else {
            detect_c_compiler(creator, executable, env, pool)
        }
    }))
}

fn detect_c_compiler<T>(
    creator: T,
    executable: PathBuf,
    env: Vec<(OsString, OsString)>,
    pool: CpuPool,
) -> SFuture<Option<Box<dyn Compiler<T>>>>
where
    T: CommandCreatorSync,
{
    trace!("detect_c_compiler");

    let test = b"#if defined(_MSC_VER) && defined(__clang__)
msvc-clang
#elif defined(_MSC_VER)
msvc
#elif defined(__clang__)
clang
#elif defined(__GNUC__)
gcc
#elif defined(__DCC__)
diab
#endif
".to_vec();
    let write = write_temp_file(&pool, "testfile.c".as_ref(), test);

    let mut cmd = creator.clone().new_command_sync(&executable);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(env.iter().map(|s| (&s.0, &s.1)));
    let output = write.and_then(move |(tempdir, src)| {
        cmd.arg("-E").arg(src);
        trace!("compiler {:?}", cmd);
        cmd.spawn()
            .and_then(|child| {
                child
                    .wait_with_output()
                    .chain_err(|| "failed to read child output")
            }).map(|e| {
                drop(tempdir);
                e
            })
    });

    Box::new(output.and_then(move |output| -> SFuture<_> {
        let stdout = match str::from_utf8(&output.stdout) {
            Ok(s) => s,
            Err(_) => return f_err("Failed to parse output"),
        };
        for line in stdout.lines() {
            //TODO: do something smarter here.
            match line {
                "clang" => {
                    debug!("Found clang");
                    return Box::new(CCompiler::new(Clang, executable, &pool)
                                    .map(|c| Some(Box::new(c) as Box<dyn Compiler<T>>)));
                }
                "diab" => {
                    debug!("Found diab");
                    return Box::new(CCompiler::new(Diab, executable, &pool)
                                    .map(|c| Some(Box::new(c) as Box<dyn Compiler<T>>)));

                }
                "gcc" => {
                    debug!("Found GCC");
                    return Box::new(CCompiler::new(GCC, executable, &pool)
                                .map(|c| Some(Box::new(c) as Box<dyn Compiler<T>>)));
                }
                "msvc" | "msvc-clang" => {
                    let is_clang = line == "msvc-clang";
                    debug!("Found MSVC (is clang: {})", is_clang);
                    let prefix = msvc::detect_showincludes_prefix(&creator,
                                                                executable.as_ref(),
                                                                is_clang,
                                                                env,
                                                                &pool);
                    return Box::new(prefix.and_then(move |prefix| {
                        trace!("showIncludes prefix: '{}'", prefix);
                        CCompiler::new(MSVC {
                            includes_prefix: prefix,
                            is_clang,
                        }, executable, &pool)
                            .map(|c| Some(Box::new(c) as Box<dyn Compiler<T>>))
                    }))
                }
                _ => (),
            }
        }

        debug!("nothing useful in detection output {:?}", stdout);
        debug!("compiler status: {}", output.status);
        debug!(
            "compiler stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        f_ok(None)
    }))
}

/// If `executable` is a known compiler, return a `Box<Compiler>` containing information about it.
pub fn get_compiler_info<T>(
    creator: &T,
    executable: &Path,
    env: &[(OsString, OsString)],
    pool: &CpuPool,
) -> SFuture<Box<dyn Compiler<T>>>
where
    T: CommandCreatorSync,
{
    let pool = pool.clone();
    let detect = detect_compiler(creator, executable, env, &pool);
    Box::new(detect.and_then(move |compiler| -> Result<_> {
        match compiler {
            Some(compiler) => Ok(compiler),
            None => bail!("could not determine compiler kind"),
        }
    }))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cache::disk::DiskCache;
    use crate::cache::Storage;
    use futures::{future, Future};
    use futures_cpupool::CpuPool;
    use crate::mock_command::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;
    use std::u64;
    use crate::test::mock_storage::MockStorage;
    use crate::test::utils::*;
    use tokio::runtime::current_thread::Runtime;

    #[test]
    fn test_detect_compiler_kind_gcc() {
        let f = TestFixture::new();
        let creator = new_creator();
        let pool = CpuPool::new(1);
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "foo\nbar\ngcc", "")),
        );
        let c = detect_compiler(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(CompilerKind::C(CCompilerKind::GCC), c.kind());
    }

    #[test]
    fn test_detect_compiler_kind_clang() {
        let f = TestFixture::new();
        let creator = new_creator();
        let pool = CpuPool::new(1);
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "clang\nfoo", "")),
        );
        let c = detect_compiler(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(CompilerKind::C(CCompilerKind::Clang), c.kind());
    }

    #[test]
    fn test_detect_compiler_kind_msvc() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let srcfile = f.touch("test.h").unwrap();
        let mut s = srcfile.to_str().unwrap();
        if s.starts_with("\\\\?\\") {
            s = &s[4..];
        }
        let prefix = String::from("blah: ");
        let stdout = format!("{}{}\r\n", prefix, s);
        // Compiler detection output
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "foo\nmsvc\nbar", "")),
        );
        // showincludes prefix detection output
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), &stdout, &String::new())),
        );
        let c = detect_compiler(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(CompilerKind::C(CCompilerKind::MSVC), c.kind());
    }

    #[test]
    fn test_detect_compiler_kind_rustc() {
        let f = TestFixture::new();
        // Windows uses bin, everything else uses lib. Just create both.
        fs::create_dir(f.tempdir.path().join("lib")).unwrap();
        fs::create_dir(f.tempdir.path().join("bin")).unwrap();
        let rustc = f.mk_bin("rustc").unwrap();
        let creator = new_creator();
        let pool = CpuPool::new(1);
        // rustc --vV
        next_command(
            &creator,
            Ok(MockChild::new(
                exit_status(0),
                "\
rustc 1.27.0 (3eda71b00 2018-06-19)
binary: rustc
commit-hash: 3eda71b00ad48d7bf4eef4c443e7f611fd061418
commit-date: 2018-06-19
host: x86_64-unknown-linux-gnu
release: 1.27.0
LLVM version: 6.0",
                "",
            )),
        );
        // rustc --print=sysroot
        let sysroot = f.tempdir.path().to_str().unwrap();
        next_command(&creator, Ok(MockChild::new(exit_status(0), &sysroot, "")));
        let c = detect_compiler(&creator, &rustc, &[], &pool)
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(CompilerKind::Rust, c.kind());
    }

    #[test]
    fn test_detect_compiler_kind_diab() {
        let f = TestFixture::new();
        let creator = new_creator();
        let pool = CpuPool::new(1);
        next_command(&creator, Ok(MockChild::new(exit_status(0), "foo\ndiab\nbar", "")));
        let c = detect_compiler(&creator, &f.bins[0], &[], &pool).wait().unwrap().unwrap();
        assert_eq!(CompilerKind::C(CCompilerKind::Diab), c.kind());
    }

    #[test]
    fn test_detect_compiler_kind_unknown() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "something", "")),
        );
        assert!(
            detect_compiler(&creator, "/foo/bar".as_ref(), &[], &pool)
                .wait()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_detect_compiler_kind_process_fail() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        next_command(&creator, Ok(MockChild::new(exit_status(1), "", "")));
        assert!(
            detect_compiler(&creator, "/foo/bar".as_ref(), &[], &pool)
                .wait()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_get_compiler_info() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        // sha-1 digest of an empty file.
        assert_eq!(CompilerKind::C(CCompilerKind::GCC), c.kind());
    }

    #[test]
    fn test_compiler_get_cached_or_compile() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let mut runtime = Runtime::new().unwrap();
        let storage = DiskCache::new(&f.tempdir.path().join("cache"), u64::MAX, &pool);
        let storage: Arc<dyn Storage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        // The preprocessor invocation.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
        );
        // The compiler invocation.
        const COMPILER_STDOUT: &'static [u8] = b"compiler stdout";
        const COMPILER_STDERR: &'static [u8] = b"compiler stderr";
        let obj = f.tempdir.path().join("foo.o");
        let o = obj.clone();
        next_command_calls(&creator, move |_| {
            // Pretend to compile something.
            let mut f = File::create(&o)?;
            f.write_all(b"file contents")?;
            Ok(MockChild::new(
                exit_status(0),
                COMPILER_STDOUT,
                COMPILER_STDERR,
            ))
        });
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        let hasher2 = hasher.clone();
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher.get_cached_or_compile(
                    Ok(None),
                    creator.clone(),
                    storage.clone(),
                    arguments.clone(),
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool.clone(),
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        match cached {
            CompileResult::CacheMiss(MissType::Normal, DistType::NoDist, _, f) => {
                // wait on cache write future so we don't race with it!
                f.wait().unwrap();
            }
            _ => assert!(false, "Unexpected compile result: {:?}", cached),
        }
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
        // Now compile again, which should be a cache hit.
        fs::remove_file(&obj).unwrap();
        // The preprocessor invocation.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
        );
        // There should be no actual compiler invocation.
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher2.get_cached_or_compile(
                    Ok(None),
                    creator.clone(),
                    storage.clone(),
                    arguments,
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool.clone(),
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        assert_eq!(CompileResult::CacheHit(Duration::new(0, 0)), cached);
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
    }

    #[test]
    #[cfg(feature = "dist-client")]
    fn test_compiler_get_cached_or_compile_dist() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let mut runtime = Runtime::new().unwrap();
        let storage = DiskCache::new(&f.tempdir.path().join("cache"), u64::MAX, &pool);
        let storage: Arc<dyn Storage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        // The preprocessor invocation.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
        );
        // The compiler invocation.
        const COMPILER_STDOUT: &'static [u8] = b"compiler stdout";
        const COMPILER_STDERR: &'static [u8] = b"compiler stderr";
        let obj = f.tempdir.path().join("foo.o");
        // Dist client will do the compilation
        let dist_client = Some(test_dist::OneshotClient::new(
            0,
            COMPILER_STDOUT.to_owned(),
            COMPILER_STDERR.to_owned(),
        ));
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        let hasher2 = hasher.clone();
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher.get_cached_or_compile(
                    Ok(dist_client.clone()),
                    creator.clone(),
                    storage.clone(),
                    arguments.clone(),
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool.clone(),
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        match cached {
            CompileResult::CacheMiss(MissType::Normal, DistType::Ok, _, f) => {
                // wait on cache write future so we don't race with it!
                f.wait().unwrap();
            }
            _ => assert!(false, "Unexpected compile result: {:?}", cached),
        }
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
        // Now compile again, which should be a cache hit.
        fs::remove_file(&obj).unwrap();
        // The preprocessor invocation.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
        );
        // There should be no actual compiler invocation.
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher2.get_cached_or_compile(
                    Ok(dist_client.clone()),
                    creator,
                    storage,
                    arguments,
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool,
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        assert_eq!(CompileResult::CacheHit(Duration::new(0, 0)), cached);
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
    }

    #[test]
    /// Test that a cache read that results in an error is treated as a cache
    /// miss.
    fn test_compiler_get_cached_or_compile_cache_error() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let mut runtime = Runtime::new().unwrap();
        let storage = MockStorage::new();
        let storage: Arc<MockStorage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        // The preprocessor invocation.
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
        );
        // The compiler invocation.
        const COMPILER_STDOUT: &'static [u8] = b"compiler stdout";
        const COMPILER_STDERR: &'static [u8] = b"compiler stderr";
        let obj = f.tempdir.path().join("foo.o");
        let o = obj.clone();
        next_command_calls(&creator, move |_| {
            // Pretend to compile something.
            let mut f = File::create(&o)?;
            f.write_all(b"file contents")?;
            Ok(MockChild::new(
                exit_status(0),
                COMPILER_STDOUT,
                COMPILER_STDERR,
            ))
        });
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        // The cache will return an error.
        storage.next_get(f_err("Some Error"));
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher.get_cached_or_compile(
                    Ok(None),
                    creator.clone(),
                    storage.clone(),
                    arguments.clone(),
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool.clone(),
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        match cached {
            CompileResult::CacheMiss(MissType::CacheReadError, DistType::NoDist, _, f) => {
                // wait on cache write future so we don't race with it!
                f.wait().unwrap();
            }
            _ => assert!(false, "Unexpected compile result: {:?}", cached),
        }

        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
    }

    #[test]
    fn test_compiler_get_cached_or_compile_force_recache() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let mut runtime = Runtime::new().unwrap();
        let storage = DiskCache::new(&f.tempdir.path().join("cache"), u64::MAX, &pool);
        let storage: Arc<dyn Storage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        const COMPILER_STDOUT: &'static [u8] = b"compiler stdout";
        const COMPILER_STDERR: &'static [u8] = b"compiler stderr";
        // The compiler should be invoked twice, since we're forcing
        // recaching.
        let obj = f.tempdir.path().join("foo.o");
        for _ in 0..2 {
            // The preprocessor invocation.
            next_command(
                &creator,
                Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
            );
            // The compiler invocation.
            let o = obj.clone();
            next_command_calls(&creator, move |_| {
                // Pretend to compile something.
                let mut f = File::create(&o)?;
                f.write_all(b"file contents")?;
                Ok(MockChild::new(
                    exit_status(0),
                    COMPILER_STDOUT,
                    COMPILER_STDERR,
                ))
            });
        }
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        let hasher2 = hasher.clone();
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher.get_cached_or_compile(
                    Ok(None),
                    creator.clone(),
                    storage.clone(),
                    arguments.clone(),
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool.clone(),
                )
            })).unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        match cached {
            CompileResult::CacheMiss(MissType::Normal, DistType::NoDist, _, f) => {
                // wait on cache write future so we don't race with it!
                f.wait().unwrap();
            }
            _ => assert!(false, "Unexpected compile result: {:?}", cached),
        }
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
        // Now compile again, but force recaching.
        fs::remove_file(&obj).unwrap();
        let (cached, res) = hasher2
            .get_cached_or_compile(
                Ok(None),
                creator,
                storage,
                arguments,
                cwd.to_path_buf(),
                vec![],
                CacheControl::ForceRecache,
                pool,
            ).wait()
            .unwrap();
        // Ensure that the object file was created.
        assert_eq!(
            true,
            fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
        );
        match cached {
            CompileResult::CacheMiss(MissType::ForcedRecache, DistType::NoDist, _, f) => {
                // wait on cache write future so we don't race with it!
                f.wait().unwrap();
            }
            _ => assert!(false, "Unexpected compile result: {:?}", cached),
        }
        assert_eq!(exit_status(0), res.status);
        assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
        assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
    }

    #[test]
    fn test_compiler_get_cached_or_compile_preprocessor_error() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let mut runtime = Runtime::new().unwrap();
        let storage = DiskCache::new(&f.tempdir.path().join("cache"), u64::MAX, &pool);
        let storage: Arc<dyn Storage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        // The preprocessor invocation.
        const PREPROCESSOR_STDERR: &'static [u8] = b"something went wrong";
        next_command(
            &creator,
            Ok(MockChild::new(
                exit_status(1),
                b"preprocessor output",
                PREPROCESSOR_STDERR,
            )),
        );
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        let (cached, res) = runtime
            .block_on(future::lazy(|| {
                hasher.get_cached_or_compile(
                    Ok(None),
                    creator,
                    storage,
                    arguments,
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::Default,
                    pool,
                )
            })).unwrap();
        assert_eq!(cached, CompileResult::Error);
        assert_eq!(exit_status(1), res.status);
        // Shouldn't get anything on stdout, since that would just be preprocessor spew!
        assert_eq!(b"", res.stdout.as_slice());
        assert_eq!(PREPROCESSOR_STDERR, res.stderr.as_slice());
    }

    #[test]
    #[cfg(feature = "dist-client")]
    fn test_compiler_get_cached_or_compile_dist_error() {
        drop(env_logger::try_init());
        let creator = new_creator();
        let f = TestFixture::new();
        let pool = CpuPool::new(1);
        let dist_clients = vec![
            test_dist::ErrorPutToolchainClient::new(),
            test_dist::ErrorAllocJobClient::new(),
            test_dist::ErrorSubmitToolchainClient::new(),
            test_dist::ErrorRunJobClient::new(),
        ];
        let storage = DiskCache::new(&f.tempdir.path().join("cache"), u64::MAX, &pool);
        let storage: Arc<dyn Storage> = Arc::new(storage);
        // Pretend to be GCC.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "gcc", "")));
        let c = get_compiler_info(&creator, &f.bins[0], &[], &pool)
            .wait()
            .unwrap();
        const COMPILER_STDOUT: &'static [u8] = b"compiler stdout";
        const COMPILER_STDERR: &'static [u8] = b"compiler stderr";
        // The compiler should be invoked twice, since we're forcing
        // recaching.
        let obj = f.tempdir.path().join("foo.o");
        for _ in dist_clients.iter() {
            // The preprocessor invocation.
            next_command(
                &creator,
                Ok(MockChild::new(exit_status(0), "preprocessor output", "")),
            );
            // The compiler invocation.
            let o = obj.clone();
            next_command_calls(&creator, move |_| {
                // Pretend to compile something.
                let mut f = File::create(&o)?;
                f.write_all(b"file contents")?;
                Ok(MockChild::new(
                    exit_status(0),
                    COMPILER_STDOUT,
                    COMPILER_STDERR,
                ))
            });
        }
        let cwd = f.tempdir.path();
        let arguments = ovec!["-c", "foo.c", "-o", "foo.o"];
        let hasher = match c.parse_arguments(&arguments, ".".as_ref()) {
            CompilerArguments::Ok(h) => h,
            o @ _ => panic!("Bad result from parse_arguments: {:?}", o),
        };
        // All these dist clients will fail, but should still result in successful compiles
        for dist_client in dist_clients {
            if obj.is_file() {
                fs::remove_file(&obj).unwrap();
            }
            let hasher = hasher.clone();
            let (cached, res) = hasher
                .get_cached_or_compile(
                    Ok(Some(dist_client.clone())),
                    creator.clone(),
                    storage.clone(),
                    arguments.clone(),
                    cwd.to_path_buf(),
                    vec![],
                    CacheControl::ForceRecache,
                    pool.clone(),
                ).wait()
                .unwrap();
            // Ensure that the object file was created.
            assert_eq!(
                true,
                fs::metadata(&obj).and_then(|m| Ok(m.len() > 0)).unwrap()
            );
            match cached {
                CompileResult::CacheMiss(MissType::ForcedRecache, DistType::Error, _, f) => {
                    // wait on cache write future so we don't race with it!
                    f.wait().unwrap();
                }
                _ => assert!(false, "Unexpected compile result: {:?}", cached),
            }
            assert_eq!(exit_status(0), res.status);
            assert_eq!(COMPILER_STDOUT, res.stdout.as_slice());
            assert_eq!(COMPILER_STDERR, res.stderr.as_slice());
        }
    }
}

#[cfg(test)]
#[cfg(feature = "dist-client")]
mod test_dist {
    use crate::dist::pkg;
    use crate::dist::{
        self, AllocJobResult, SchedulerStatusResult, CompileCommand, JobAlloc, JobComplete,
        JobId, OutputData, PathTransformer, ProcessOutput, RunJobResult, ServerId,
        SubmitToolchainResult, Toolchain,
    };
    use std::cell::Cell;
    use std::path::Path;
    use std::sync::Arc;

    use crate::errors::*;

    pub struct ErrorPutToolchainClient;
    impl ErrorPutToolchainClient {
        pub fn new() -> Arc<dyn dist::Client> {
            Arc::new(ErrorPutToolchainClient)
        }
    }
    impl dist::Client for ErrorPutToolchainClient {
        fn do_alloc_job(&self, _: Toolchain) -> SFuture<AllocJobResult> {
            unreachable!()
        }
        fn do_get_status(&self) -> SFuture<SchedulerStatusResult> {
            unreachable!()
        }
        fn do_submit_toolchain(&self, _: JobAlloc, _: Toolchain) -> SFuture<SubmitToolchainResult> {
            unreachable!()
        }
        fn do_run_job(
            &self,
            _: JobAlloc,
            _: CompileCommand,
            _: Vec<String>,
            _: Box<dyn pkg::InputsPackager>,
        ) -> SFuture<(RunJobResult, PathTransformer)> {
            unreachable!()
        }
        fn put_toolchain(
            &self,
            _: &Path,
            _: &str,
            _: Box<dyn pkg::ToolchainPackager>,
        ) -> SFuture<(Toolchain, Option<String>)> {
            f_err("put toolchain failure")
        }
    }

    pub struct ErrorAllocJobClient {
        tc: Toolchain,
    }
    impl ErrorAllocJobClient {
        pub fn new() -> Arc<dyn dist::Client> {
            Arc::new(Self {
                tc: Toolchain {
                    archive_id: "somearchiveid".to_owned(),
                },
            })
        }
    }
    impl dist::Client for ErrorAllocJobClient {
        fn do_alloc_job(&self, tc: Toolchain) -> SFuture<AllocJobResult> {
            assert_eq!(self.tc, tc);
            f_err("alloc job failure")
        }
        fn do_get_status(&self) -> SFuture<SchedulerStatusResult> {
            unreachable!()
        }
        fn do_submit_toolchain(&self, _: JobAlloc, _: Toolchain) -> SFuture<SubmitToolchainResult> {
            unreachable!()
        }
        fn do_run_job(
            &self,
            _: JobAlloc,
            _: CompileCommand,
            _: Vec<String>,
            _: Box<dyn pkg::InputsPackager>,
        ) -> SFuture<(RunJobResult, PathTransformer)> {
            unreachable!()
        }
        fn put_toolchain(
            &self,
            _: &Path,
            _: &str,
            _: Box<dyn pkg::ToolchainPackager>,
        ) -> SFuture<(Toolchain, Option<String>)> {
            f_ok((self.tc.clone(), None))
        }
    }

    pub struct ErrorSubmitToolchainClient {
        has_started: Cell<bool>,
        tc: Toolchain,
    }
    impl ErrorSubmitToolchainClient {
        pub fn new() -> Arc<dyn dist::Client> {
            Arc::new(Self {
                has_started: Cell::new(false),
                tc: Toolchain {
                    archive_id: "somearchiveid".to_owned(),
                },
            })
        }
    }
    impl dist::Client for ErrorSubmitToolchainClient {
        fn do_alloc_job(&self, tc: Toolchain) -> SFuture<AllocJobResult> {
            assert!(!self.has_started.replace(true));
            assert_eq!(self.tc, tc);
            f_ok(AllocJobResult::Success {
                job_alloc: JobAlloc {
                    auth: "abcd".to_owned(),
                    job_id: JobId(0),
                    server_id: ServerId::new(([0, 0, 0, 0], 1).into()),
                },
                need_toolchain: true,
            })
        }
        fn do_get_status(&self) -> SFuture<SchedulerStatusResult> {
            unreachable!()
        }
        fn do_submit_toolchain(
            &self,
            job_alloc: JobAlloc,
            tc: Toolchain,
        ) -> SFuture<SubmitToolchainResult> {
            assert_eq!(job_alloc.job_id, JobId(0));
            assert_eq!(self.tc, tc);
            f_err("submit toolchain failure")
        }
        fn do_run_job(
            &self,
            _: JobAlloc,
            _: CompileCommand,
            _: Vec<String>,
            _: Box<dyn pkg::InputsPackager>,
        ) -> SFuture<(RunJobResult, PathTransformer)> {
            unreachable!()
        }
        fn put_toolchain(
            &self,
            _: &Path,
            _: &str,
            _: Box<dyn pkg::ToolchainPackager>,
        ) -> SFuture<(Toolchain, Option<String>)> {
            f_ok((self.tc.clone(), None))
        }
    }

    pub struct ErrorRunJobClient {
        has_started: Cell<bool>,
        tc: Toolchain,
    }
    impl ErrorRunJobClient {
        pub fn new() -> Arc<dyn dist::Client> {
            Arc::new(Self {
                has_started: Cell::new(false),
                tc: Toolchain {
                    archive_id: "somearchiveid".to_owned(),
                },
            })
        }
    }
    impl dist::Client for ErrorRunJobClient {
        fn do_alloc_job(&self, tc: Toolchain) -> SFuture<AllocJobResult> {
            assert!(!self.has_started.replace(true));
            assert_eq!(self.tc, tc);
            f_ok(AllocJobResult::Success {
                job_alloc: JobAlloc {
                    auth: "abcd".to_owned(),
                    job_id: JobId(0),
                    server_id: ServerId::new(([0, 0, 0, 0], 1).into()),
                },
                need_toolchain: true,
            })
        }
        fn do_get_status(&self) -> SFuture<SchedulerStatusResult> {
            unreachable!()
        }
        fn do_submit_toolchain(
            &self,
            job_alloc: JobAlloc,
            tc: Toolchain,
        ) -> SFuture<SubmitToolchainResult> {
            assert_eq!(job_alloc.job_id, JobId(0));
            assert_eq!(self.tc, tc);
            f_ok(SubmitToolchainResult::Success)
        }
        fn do_run_job(
            &self,
            job_alloc: JobAlloc,
            command: CompileCommand,
            _: Vec<String>,
            _: Box<dyn pkg::InputsPackager>,
        ) -> SFuture<(RunJobResult, PathTransformer)> {
            assert_eq!(job_alloc.job_id, JobId(0));
            assert_eq!(command.executable, "/overridden/compiler");
            f_err("run job failure")
        }
        fn put_toolchain(
            &self,
            _: &Path,
            _: &str,
            _: Box<dyn pkg::ToolchainPackager>,
        ) -> SFuture<(Toolchain, Option<String>)> {
            f_ok((self.tc.clone(), Some("/overridden/compiler".to_owned())))
        }
    }

    pub struct OneshotClient {
        has_started: Cell<bool>,
        tc: Toolchain,
        output: ProcessOutput,
    }

    impl OneshotClient {
        pub fn new(code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Arc<dyn dist::Client> {
            Arc::new(Self {
                has_started: Cell::new(false),
                tc: Toolchain {
                    archive_id: "somearchiveid".to_owned(),
                },
                output: ProcessOutput::fake_output(code, stdout, stderr),
            })
        }
    }

    impl dist::Client for OneshotClient {
        fn do_alloc_job(&self, tc: Toolchain) -> SFuture<AllocJobResult> {
            assert!(!self.has_started.replace(true));
            assert_eq!(self.tc, tc);

            f_ok(AllocJobResult::Success {
                job_alloc: JobAlloc {
                    auth: "abcd".to_owned(),
                    job_id: JobId(0),
                    server_id: ServerId::new(([0, 0, 0, 0], 1).into()),
                },
                need_toolchain: true,
            })
        }
        fn do_get_status(&self) -> SFuture<SchedulerStatusResult> {
            unreachable!()
        }
        fn do_submit_toolchain(
            &self,
            job_alloc: JobAlloc,
            tc: Toolchain,
        ) -> SFuture<SubmitToolchainResult> {
            assert_eq!(job_alloc.job_id, JobId(0));
            assert_eq!(self.tc, tc);

            f_ok(SubmitToolchainResult::Success)
        }
        fn do_run_job(
            &self,
            job_alloc: JobAlloc,
            command: CompileCommand,
            outputs: Vec<String>,
            inputs_packager: Box<dyn pkg::InputsPackager>,
        ) -> SFuture<(RunJobResult, PathTransformer)> {
            assert_eq!(job_alloc.job_id, JobId(0));
            assert_eq!(command.executable, "/overridden/compiler");

            let mut inputs = vec![];
            let path_transformer = inputs_packager.write_inputs(&mut inputs).unwrap();
            let outputs = outputs
                .into_iter()
                .map(|name| {
                    let data = format!("some data in {}", name);
                    let data = OutputData::try_from_reader(data.as_bytes()).unwrap();
                    (name, data)
                }).collect();
            let result = RunJobResult::Complete(JobComplete {
                output: self.output.clone(),
                outputs,
            });
            f_ok((result, path_transformer))
        }
        fn put_toolchain(
            &self,
            _: &Path,
            _: &str,
            _: Box<dyn pkg::ToolchainPackager>,
        ) -> SFuture<(Toolchain, Option<String>)> {
            f_ok((self.tc.clone(), Some("/overridden/compiler".to_owned())))
        }
    }
}
