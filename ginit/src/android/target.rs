// TODO: Bad things happen if multiple Android devices are connected at once

use super::ndk;
use crate::{
    config::Config,
    init::cargo::CargoTarget,
    target::{Profile, TargetTrait},
    util::{self, force_symlink},
};
use into_result::{command::CommandResult, IntoResult as _};
use std::{collections::BTreeMap, fs, io, path::PathBuf, process::Command};

const API_VERSION: u32 = 24;

fn so_name(config: &Config) -> String {
    format!("lib{}.so", config.app_name())
}

fn gradlew(config: &Config) -> Command {
    let gradlew_path = config.android().project_path().join("gradlew");
    let mut command = Command::new(&gradlew_path);
    command.arg("--project-dir");
    command.arg(config.android().project_path());
    command
}

#[derive(Clone, Copy, Debug)]
enum CargoMode {
    Check,
    Build,
}

impl CargoMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CargoMode::Check => "check",
            CargoMode::Build => "build",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub triple: &'a str,
    clang_triple_override: Option<&'a str>,
    binutils_triple_override: Option<&'a str>,
    pub abi: &'a str,
    pub arch: &'a str,
}

impl<'a> TargetTrait<'a> for Target<'a> {
    fn all() -> &'a BTreeMap<&'a str, Self> {
        lazy_static::lazy_static! {
            static ref TARGETS: BTreeMap<&'static str, Target<'static>> = {
                let mut targets = BTreeMap::new();
                targets.insert("aarch64", Target {
                    triple: "aarch64-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "arm64-v8a",
                    arch: "arm64",
                });
                targets.insert("armv7", Target {
                    triple: "armv7-linux-androideabi",
                    clang_triple_override: Some("armv7a-linux-androideabi"),
                    binutils_triple_override: Some("arm-linux-androideabi"),
                    abi: "armeabi-v7a",
                    arch: "arm",
                });
                targets.insert("i686", Target {
                    triple: "i686-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "x86",
                    arch: "x86",
                });
                targets.insert("x86_64", Target {
                    triple: "x86_64-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "x86_64",
                    arch: "x86_64",
                });
                targets
            };
        }
        &*TARGETS
    }

    fn triple(&'a self) -> &'a str {
        self.triple
    }

    fn arch(&'a self) -> &'a str {
        self.arch
    }
}

impl<'a> Target<'a> {
    fn clang_triple(&self) -> &'a str {
        self.clang_triple_override.unwrap_or_else(|| self.triple)
    }

    fn binutils_triple(&self) -> &'a str {
        self.binutils_triple_override.unwrap_or_else(|| self.triple)
    }

    fn for_abi(abi: &str) -> Option<&'a Self> {
        Self::all().values().find(|target| target.abi == abi)
    }

    pub fn for_connected() -> CommandResult<Option<&'a Self>> {
        let output = Command::new("adb")
            .args(&["shell", "getprop", "ro.product.cpu.abi"])
            .output()
            .into_result()?;
        let raw_abi = String::from_utf8(output.stdout)
            .expect("`ro.product.cpu.abi` contained invalid unicode");
        let abi = raw_abi.trim();
        Ok(Self::for_abi(abi))
    }

    pub fn generate_cargo_config(&self, ndk_env: &ndk::Env) -> CargoTarget {
        let ar = ndk_env
            .binutil_path(ndk::Binutil::Ar, self.binutils_triple())
            .expect("couldn't find ar")
            .display()
            .to_string();
        // Using clang as the linker seems to be the only way to get the right library search paths...
        let linker = ndk_env
            .compiler_path(ndk::Compiler::Clang, self.clang_triple(), API_VERSION)
            .expect("couldn't find clang")
            .display()
            .to_string();
        CargoTarget {
            ar: Some(ar),
            linker: Some(linker),
            rustflags: vec![
                "-C".to_owned(),
                "link-arg=-landroid".to_owned(),
                "-C".to_owned(),
                "link-arg=-llog".to_owned(),
                "-C".to_owned(),
                "link-arg=-lOpenSLES".to_owned(),
            ],
        }
    }

    fn compile_lib(
        &self,
        config: &Config,
        ndk_env: &ndk::Env,
        verbose: bool,
        profile: Profile,
        mode: CargoMode,
    ) {
        util::CargoCommand::new(mode.as_str())
            .with_verbose(verbose)
            .with_package(Some(config.app_name()))
            .with_manifest_path(config.manifest_path())
            .with_target(Some(self.triple))
            .with_features(Some("vulkan")) // TODO: rust-lib plugin
            .with_release(profile.is_release())
            .into_command()
            .env("ANDROID_NATIVE_API_LEVEL", API_VERSION.to_string())
            .env(
                "TARGET_AR",
                ndk_env
                    .binutil_path(ndk::Binutil::Ar, self.binutils_triple())
                    .expect("couldn't find ar"),
            )
            .env(
                "TARGET_CC",
                ndk_env
                    .compiler_path(ndk::Compiler::Clang, self.clang_triple(), API_VERSION)
                    .expect("couldn't find clang"),
            )
            .env(
                "TARGET_CXX",
                ndk_env
                    .compiler_path(ndk::Compiler::Clangxx, self.clang_triple(), API_VERSION)
                    .expect("couldn't find clang++"),
            )
            .status()
            .into_result()
            .expect("Failed to run `cargo build`");
    }

    fn get_jnilibs_subdir(&self, config: &Config) -> PathBuf {
        config
            .android()
            .project_path()
            .join(format!("app/src/main/jniLibs/{}", &self.abi))
    }

    fn make_jnilibs_subdir(&self, config: &Config) -> Result<(), io::Error> {
        let path = self.get_jnilibs_subdir(config);
        fs::create_dir_all(path)
    }

    fn symlink_lib(&self, config: &Config, profile: Profile) {
        self.make_jnilibs_subdir(config)
            .expect("Failed to create jniLibs subdir");
        let so_name = so_name(config);
        let src = config.prefix_path(format!(
            "target/{}/{}/{}",
            &self.triple,
            profile.as_str(),
            &so_name
        ));
        if !src.exists() {
            panic!("Symlink source doesn't exist: {:?}", src);
        }
        let dest = self.get_jnilibs_subdir(config).join(&so_name);
        force_symlink(src, dest).expect("Failed to symlink lib");
    }

    pub fn check(&self, config: &Config, ndk_env: &ndk::Env, verbose: bool) {
        self.compile_lib(config, ndk_env, verbose, Profile::Debug, CargoMode::Check);
    }

    pub fn build(&self, config: &Config, ndk_env: &ndk::Env, verbose: bool, profile: Profile) {
        self.compile_lib(config, ndk_env, verbose, profile, CargoMode::Build);
        self.symlink_lib(config, profile);
    }

    fn clean_jnilibs(config: &Config) {
        for target in Self::all().values() {
            let link = target.get_jnilibs_subdir(config).join(so_name(config));
            if let Ok(path) = fs::read_link(&link) {
                if !path.exists() {
                    log::info!(
                        "deleting broken symlink {:?} (points to {:?}, which doesn't exist)",
                        link,
                        path
                    );
                    fs::remove_file(link).expect("Failed to delete broken symlink");
                }
            }
        }
    }

    fn build_and_install(
        &self,
        config: &Config,
        ndk_env: &ndk::Env,
        verbose: bool,
        profile: Profile,
    ) {
        Self::clean_jnilibs(config);
        self.build(config, ndk_env, verbose, profile);
        gradlew(config)
            .arg("installDebug")
            .status()
            .into_result()
            .expect("Failed to build and install APK");
    }

    fn wake_screen(&self) {
        Command::new("adb")
            .args(&["shell", "input", "keyevent", "KEYCODE_WAKEUP"])
            .status()
            .into_result()
            .expect("Failed to wake device screen");
    }

    pub fn run(&self, config: &Config, ndk_env: &ndk::Env, verbose: bool, profile: Profile) {
        self.build_and_install(config, ndk_env, verbose, profile);
        let activity = format!(
            "{}.{}/android.app.NativeActivity",
            config.reverse_domain(),
            config.app_name(),
        );
        Command::new("adb")
            .args(&["shell", "am", "start", "-n", &activity])
            .status()
            .into_result()
            .expect("Failed to start APK on device");
        self.wake_screen();
    }

    pub fn stacktrace(&self, config: &Config, ndk_env: &ndk::Env) {
        let mut logcat_command = Command::new("adb");
        logcat_command.args(&["logcat", "-d"]); // print and exit
        let mut stack_command = Command::new("ndk-stack");
        stack_command
            .env("PATH", util::add_to_path(ndk_env.home().display()))
            .arg("-sym")
            .arg(self.get_jnilibs_subdir(config));
        util::pipe(logcat_command, stack_command).expect("Failed to get stacktrace");
    }
}