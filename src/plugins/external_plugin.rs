use std::collections::{BTreeMap, HashMap};
use std::fmt::{Debug, Formatter};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::exit;

use clap::Command;
use color_eyre::eyre::{eyre, Result, WrapErr};
use console::style;
use itertools::Itertools;
use once_cell::sync::Lazy;

use crate::cache::CacheManager;
use crate::config::{Config, Settings};
use crate::env::RTX_FETCH_REMOTE_VERSIONS_TIMEOUT;
use crate::env_diff::{EnvDiff, EnvDiffOperation};
use crate::errors::Error::PluginNotInstalled;
use crate::file::remove_all;
use crate::git::Git;
use crate::hash::hash_to_str;
use crate::plugins::external_plugin_cache::ExternalPluginCache;
use crate::plugins::rtx_plugin_toml::RtxPluginToml;
use crate::plugins::Script::{Download, ExecEnv, Install, ParseLegacyFile};
use crate::plugins::{Plugin, PluginName, PluginType, Script, ScriptManager};
use crate::timeout::run_with_timeout;
use crate::toolset::{ToolVersion, ToolVersionRequest};
use crate::ui::multi_progress_report::MultiProgressReport;
use crate::ui::progress_report::ProgressReport;
use crate::ui::prompt;
use crate::{dirs, env, file};

/// This represents a plugin installed to ~/.local/share/rtx/plugins
pub struct ExternalPlugin {
    pub name: PluginName,
    pub plugin_path: PathBuf,
    pub repo_url: Option<String>,
    pub toml: RtxPluginToml,
    cache_path: PathBuf,
    downloads_path: PathBuf,
    installs_path: PathBuf,
    script_man: ScriptManager,
    cache: ExternalPluginCache,
    remote_version_cache: CacheManager<Vec<String>>,
    latest_stable_cache: CacheManager<Option<String>>,
    alias_cache: CacheManager<Vec<(String, String)>>,
    legacy_filename_cache: CacheManager<Vec<String>>,
}

impl ExternalPlugin {
    pub fn new(name: PluginName) -> Self {
        let plugin_path = dirs::PLUGINS.join(&name);
        let cache_path = dirs::CACHE.join(&name);
        let toml_path = plugin_path.join("rtx.plugin.toml");
        let toml = RtxPluginToml::from_file(&toml_path).unwrap();
        Self {
            script_man: build_script_man(&name, &plugin_path),
            downloads_path: dirs::DOWNLOADS.join(&name),
            installs_path: dirs::INSTALLS.join(&name),
            cache: ExternalPluginCache::default(),
            remote_version_cache: CacheManager::new(cache_path.join("remote_versions.msgpack.z"))
                .with_fresh_duration(*env::RTX_FETCH_REMOTE_VERSIONS_CACHE)
                .with_fresh_file(plugin_path.clone())
                .with_fresh_file(plugin_path.join("bin/list-all")),
            latest_stable_cache: CacheManager::new(cache_path.join("latest_stable.msgpack.z"))
                .with_fresh_duration(*env::RTX_FETCH_REMOTE_VERSIONS_CACHE)
                .with_fresh_file(plugin_path.clone())
                .with_fresh_file(plugin_path.join("bin/latest-stable")),
            alias_cache: CacheManager::new(cache_path.join("aliases.msgpack.z"))
                .with_fresh_file(plugin_path.clone())
                .with_fresh_file(plugin_path.join("bin/list-aliases")),
            legacy_filename_cache: CacheManager::new(cache_path.join("legacy_filenames.msgpack.z"))
                .with_fresh_file(plugin_path.clone())
                .with_fresh_file(plugin_path.join("bin/list-legacy-filenames")),
            plugin_path,
            cache_path,
            repo_url: None,
            toml,
            name,
        }
    }

    fn get_repo_url(&self, config: &Config) -> Result<String> {
        self.repo_url
            .clone()
            .or_else(|| config.get_repo_url(&self.name))
            .ok_or_else(|| eyre!("No repository found for plugin {}", self.name))
    }

    fn install(&self, config: &Config, pr: &ProgressReport) -> Result<()> {
        let repository = self.get_repo_url(config)?;
        let (repo_url, repo_ref) = Git::split_url_and_ref(&repository);
        debug!("install {} {:?}", self.name, repository);

        if self.is_installed() {
            self.uninstall(pr)?;
        }

        let git = Git::new(self.plugin_path.to_path_buf());
        pr.set_message(format!("cloning {repo_url}"));
        git.clone(&repo_url)?;
        if let Some(ref_) = &repo_ref {
            pr.set_message(format!("checking out {ref_}"));
            git.update(Some(ref_.to_string()))?;
        }

        pr.set_message("loading plugin remote versions");
        if self.has_list_all_script() {
            self.list_remote_versions(&config.settings)?;
        }
        if self.has_list_alias_script() {
            pr.set_message("getting plugin aliases");
            self.get_aliases(&config.settings)?;
        }
        if self.has_list_legacy_filenames_script() {
            pr.set_message("getting plugin legacy filenames");
            self.legacy_filenames(&config.settings)?;
        }

        let sha = git.current_sha_short()?;
        pr.finish_with_message(format!(
            "{repo_url}#{}",
            style(&sha).bright().yellow().for_stderr(),
        ));
        Ok(())
    }

    fn fetch_remote_versions(&self, settings: &Settings) -> Result<Vec<String>> {
        let cmd = self.script_man.cmd(settings, &Script::ListAll);
        let result = run_with_timeout(
            move || {
                let result = cmd.stdout_capture().stderr_capture().unchecked().run()?;
                Ok(result)
            },
            *RTX_FETCH_REMOTE_VERSIONS_TIMEOUT,
        )
        .map_err(|err| {
            let script = self.script_man.get_script_path(&Script::ListAll);
            eyre!("Failed to run {}: {}", script.display(), err)
        })?;
        let stdout = String::from_utf8(result.stdout).unwrap();
        let stderr = String::from_utf8(result.stderr).unwrap().trim().to_string();

        let display_stderr = || {
            if !stderr.is_empty() {
                eprintln!("{stderr}");
            }
        };
        if !result.status.success() {
            return Err(eyre!(
                "error running {}: exited with code {}\n{}",
                Script::ListAll,
                result.status.code().unwrap_or_default(),
                stderr
            ))?;
        } else if settings.verbose {
            display_stderr();
        }

        Ok(stdout.split_whitespace().map(|v| v.into()).collect())
    }

    fn fetch_legacy_filenames(&self, settings: &Settings) -> Result<Vec<String>> {
        let stdout = self
            .script_man
            .read(settings, &Script::ListLegacyFilenames)?;
        Ok(self.parse_legacy_filenames(&stdout))
    }
    fn parse_legacy_filenames(&self, data: &str) -> Vec<String> {
        data.split_whitespace().map(|v| v.into()).collect()
    }
    fn fetch_latest_stable(&self, settings: &Settings) -> Result<Option<String>> {
        let latest_stable = self
            .script_man
            .read(settings, &Script::LatestStable)?
            .trim()
            .to_string();
        Ok(if latest_stable.is_empty() {
            None
        } else {
            Some(latest_stable)
        })
    }

    fn has_list_all_script(&self) -> bool {
        self.script_man.script_exists(&Script::ListAll)
    }
    fn has_list_alias_script(&self) -> bool {
        self.script_man.script_exists(&Script::ListAliases)
    }
    fn has_list_legacy_filenames_script(&self) -> bool {
        self.script_man.script_exists(&Script::ListLegacyFilenames)
    }
    fn has_latest_stable_script(&self) -> bool {
        self.script_man.script_exists(&Script::LatestStable)
    }
    fn fetch_aliases(&self, settings: &Settings) -> Result<Vec<(String, String)>> {
        let stdout = self.script_man.read(settings, &Script::ListAliases)?;
        Ok(self.parse_aliases(&stdout))
    }
    fn parse_aliases(&self, data: &str) -> Vec<(String, String)> {
        data.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace().collect_vec();
                if parts.len() != 2 {
                    if !parts.is_empty() {
                        trace!("invalid alias line: {}", line);
                    }
                    return None;
                }
                Some((parts.remove(0).into(), parts.remove(0).into()))
            })
            .collect()
    }

    fn fetch_cached_legacy_file(&self, legacy_file: &Path) -> Result<Option<String>> {
        let fp = self.legacy_cache_file_path(legacy_file);
        if !fp.exists() || fp.metadata()?.modified()? < legacy_file.metadata()?.modified()? {
            return Ok(None);
        }

        Ok(Some(fs::read_to_string(fp)?.trim().into()))
    }

    fn legacy_cache_file_path(&self, legacy_file: &Path) -> PathBuf {
        self.cache_path
            .join("legacy")
            .join(&self.name)
            .join(hash_to_str(&legacy_file.to_string_lossy()))
            .with_extension("txt")
    }

    fn write_legacy_cache(&self, legacy_file: &Path, legacy_version: &str) -> Result<()> {
        let fp = self.legacy_cache_file_path(legacy_file);
        file::create_dir_all(fp.parent().unwrap())?;
        file::write(fp, legacy_version)?;
        Ok(())
    }

    fn fetch_bin_paths(&self, config: &Config, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        let list_bin_paths = self.plugin_path.join("bin/list-bin-paths");
        let bin_paths = if matches!(tv.request, ToolVersionRequest::System(_)) {
            Vec::new()
        } else if list_bin_paths.exists() {
            let output = self
                .script_man_for_tv(config, tv)
                .cmd(&config.settings, &Script::ListBinPaths)
                .read()?;
            output.split_whitespace().map(|f| f.to_string()).collect()
        } else {
            vec!["bin".into()]
        };
        let bin_paths = bin_paths
            .into_iter()
            .map(|path| tv.install_path().join(path))
            .collect();
        Ok(bin_paths)
    }
    fn fetch_exec_env(&self, config: &Config, tv: &ToolVersion) -> Result<HashMap<String, String>> {
        let script = self.script_man_for_tv(config, tv).get_script_path(&ExecEnv);
        let ed = EnvDiff::from_bash_script(&script, &self.script_man_for_tv(config, tv).env)?;
        let env = ed
            .to_patches()
            .into_iter()
            .filter_map(|p| match p {
                EnvDiffOperation::Add(key, value) => Some((key, value)),
                EnvDiffOperation::Change(key, value) => Some((key, value)),
                _ => None,
            })
            .collect();
        Ok(env)
    }

    fn script_man_for_tv(&self, config: &Config, tv: &ToolVersion) -> ScriptManager {
        let mut sm = self.script_man.clone();
        for (key, value) in &tv.opts {
            let k = format!("RTX_TOOL_OPTS__{}", key.to_uppercase());
            sm = sm.with_env(k, value.clone());
        }
        if let Some(project_root) = &config.project_root {
            let project_root = project_root.to_string_lossy().to_string();
            sm = sm.with_env("RTX_PROJECT_ROOT", project_root);
        }
        let install_type = match &tv.request {
            ToolVersionRequest::Version(_, _) | ToolVersionRequest::Prefix(_, _) => "version",
            ToolVersionRequest::Ref(_, _) => "ref",
            ToolVersionRequest::Path(_, _) => "path",
            ToolVersionRequest::Sub { .. } => "sub",
            ToolVersionRequest::System(_) => {
                panic!("should not be called for system tool")
            }
        };
        let install_version = match &tv.request {
            ToolVersionRequest::Ref(_, v) => v, // should not have "ref:" prefix
            _ => &tv.version,
        };
        sm = sm
            .with_env(
                "RTX_INSTALL_PATH",
                tv.install_path().to_string_lossy().to_string(),
            )
            .with_env(
                "ASDF_INSTALL_PATH",
                tv.install_path().to_string_lossy().to_string(),
            )
            .with_env(
                "RTX_DOWNLOAD_PATH",
                tv.download_path().to_string_lossy().to_string(),
            )
            .with_env(
                "ASDF_DOWNLOAD_PATH",
                tv.download_path().to_string_lossy().to_string(),
            )
            .with_env("RTX_INSTALL_TYPE", install_type)
            .with_env("ASDF_INSTALL_TYPE", install_type)
            .with_env("RTX_INSTALL_VERSION", install_version)
            .with_env("ASDF_INSTALL_VERSION", install_version);
        sm
    }
}

fn build_script_man(name: &str, plugin_path: &Path) -> ScriptManager {
    ScriptManager::new(plugin_path.to_path_buf())
        .with_env("RTX_PLUGIN_NAME", name.to_string())
        .with_env("RTX_PLUGIN_PATH", plugin_path.to_string_lossy().to_string())
        .with_env("RTX_SHIMS_DIR", &*dirs::SHIMS)
}

impl Eq for ExternalPlugin {}

impl PartialEq for ExternalPlugin {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Hash for ExternalPlugin {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Plugin for ExternalPlugin {
    fn name(&self) -> &PluginName {
        &self.name
    }

    fn get_type(&self) -> PluginType {
        PluginType::External
    }
    fn list_remote_versions(&self, settings: &Settings) -> Result<Vec<String>> {
        self.remote_version_cache
            .get_or_try_init(|| self.fetch_remote_versions(settings))
            .map_err(|err| {
                eyre!(
                    "Failed listing remote versions for plugin {}: {}",
                    style(&self.name).cyan().for_stderr(),
                    err
                )
            })
            .cloned()
    }

    fn latest_stable_version(&self, settings: &Settings) -> Result<Option<String>> {
        if !self.has_latest_stable_script() {
            return Ok(None);
        }
        self.latest_stable_cache
            .get_or_try_init(|| self.fetch_latest_stable(settings))
            .map_err(|err| {
                eyre!(
                    "Failed fetching latest stable version for plugin {}: {}",
                    style(&self.name).cyan().for_stderr(),
                    err
                )
            })
            .cloned()
    }

    fn get_remote_url(&self) -> Option<String> {
        let git = Git::new(self.plugin_path.to_path_buf());
        git.get_remote_url().or_else(|| self.repo_url.clone())
    }

    fn current_sha_short(&self) -> Result<String> {
        let git = Git::new(self.plugin_path.to_path_buf());
        git.current_sha_short()
    }

    fn current_abbrev_ref(&self) -> Result<String> {
        let git = Git::new(self.plugin_path.to_path_buf());
        git.current_abbrev_ref()
    }

    fn is_installed(&self) -> bool {
        self.plugin_path.exists()
    }

    fn ensure_installed(
        &self,
        config: &mut Config,
        mpr: Option<&MultiProgressReport>,
        force: bool,
    ) -> Result<()> {
        if !force {
            if self.is_installed() {
                return Ok(());
            }
            if !config.settings.yes && self.repo_url.is_none() {
                let url = self.get_repo_url(config)?;
                eprintln!(
                    "⚠️  {name} is a community-developed plugin: {url}",
                    name = style(&self.name).cyan(),
                    url = style(url.trim_end_matches(".git")).yellow(),
                );
                if !prompt::confirm(&format!("Would you like to install {}?", self.name))? {
                    Err(PluginNotInstalled(self.name.clone()))?
                }
            }
        }
        let _mpr = MultiProgressReport::new(config.show_progress_bars());
        let mpr = mpr.unwrap_or(&_mpr);
        let mut pr = mpr.add();
        self.decorate_progress_bar(&mut pr, None);
        let _lock = self.get_lock(&self.plugin_path, force)?;
        self.install(config, &pr)
    }

    fn update(&self, gitref: Option<String>) -> Result<()> {
        let plugin_path = self.plugin_path.to_path_buf();
        if plugin_path.is_symlink() {
            warn!(
                "Plugin: {} is a symlink, not updating",
                style(&self.name).cyan().for_stderr()
            );
            return Ok(());
        }
        let git = Git::new(plugin_path);
        if !git.is_repo() {
            warn!(
                "Plugin {} is not a git repository, not updating",
                style(&self.name).cyan().for_stderr()
            );
            return Ok(());
        }
        // TODO: asdf_run_hook "pre_plugin_update"
        let (_pre, _post) = git.update(gitref)?;
        // TODO: asdf_run_hook "post_plugin_update"
        Ok(())
    }

    fn uninstall(&self, pr: &ProgressReport) -> Result<()> {
        if !self.is_installed() {
            return Ok(());
        }
        pr.set_message("uninstalling");

        let rmdir = |dir: &Path| {
            if !dir.exists() {
                return Ok(());
            }
            pr.set_message(format!("removing {}", &dir.to_string_lossy()));
            remove_all(dir).wrap_err_with(|| {
                format!(
                    "Failed to remove directory {}",
                    style(&dir.to_string_lossy()).cyan().for_stderr()
                )
            })
        };

        rmdir(&self.plugin_path)?;

        Ok(())
    }

    fn get_aliases(&self, settings: &Settings) -> Result<BTreeMap<String, String>> {
        if let Some(data) = &self.toml.list_aliases.data {
            return Ok(self.parse_aliases(data).into_iter().collect());
        }
        if !self.has_list_alias_script() {
            return Ok(BTreeMap::new());
        }
        let aliases = self
            .alias_cache
            .get_or_try_init(|| self.fetch_aliases(settings))
            .map_err(|err| {
                eyre!(
                    "Failed fetching aliases for plugin {}: {}",
                    style(&self.name).cyan().for_stderr(),
                    err
                )
            })?
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Ok(aliases)
    }

    fn legacy_filenames(&self, settings: &Settings) -> Result<Vec<String>> {
        if let Some(data) = &self.toml.list_legacy_filenames.data {
            return Ok(self.parse_legacy_filenames(data));
        }
        if !self.has_list_legacy_filenames_script() {
            return Ok(vec![]);
        }
        self.legacy_filename_cache
            .get_or_try_init(|| self.fetch_legacy_filenames(settings))
            .map_err(|err| {
                eyre!(
                    "Failed fetching legacy filenames for plugin {}: {}",
                    style(&self.name).cyan().for_stderr(),
                    err
                )
            })
            .cloned()
    }

    fn parse_legacy_file(&self, legacy_file: &Path, settings: &Settings) -> Result<String> {
        if let Some(cached) = self.fetch_cached_legacy_file(legacy_file)? {
            return Ok(cached);
        }
        trace!("parsing legacy file: {}", legacy_file.to_string_lossy());
        let script = ParseLegacyFile(legacy_file.to_string_lossy().into());
        let legacy_version = match self.script_man.script_exists(&script) {
            true => self.script_man.read(settings, &script)?,
            false => fs::read_to_string(legacy_file)?,
        }
        .trim()
        .to_string();

        self.write_legacy_cache(legacy_file, &legacy_version)?;
        Ok(legacy_version)
    }

    fn external_commands(&self) -> Result<Vec<Command>> {
        let command_path = self.plugin_path.join("lib/commands");
        if !self.is_installed() || !command_path.exists() || self.name == "direnv" {
            // asdf-direnv is disabled since it conflicts with rtx's built-in direnv functionality
            return Ok(vec![]);
        }
        let mut commands = vec![];
        for command in file::dir_files(&command_path)? {
            if !command.starts_with("command-") || !command.ends_with(".bash") {
                continue;
            }
            let command = command
                .strip_prefix("command-")
                .unwrap()
                .strip_suffix(".bash")
                .unwrap()
                .split('-')
                .map(|s| s.to_string())
                .collect::<Vec<String>>();
            commands.push(command);
        }
        if commands.is_empty() {
            return Ok(vec![]);
        }

        let topic = Command::new(self.name.clone())
            .about(format!("Commands provided by {} plugin", &self.name))
            .subcommands(commands.into_iter().map(|cmd| {
                Command::new(cmd.join("-"))
                    .about(format!("{} command", cmd.join("-")))
                    .arg(
                        clap::Arg::new("args")
                            .num_args(1..)
                            .allow_hyphen_values(true)
                            .trailing_var_arg(true),
                    )
            }));
        Ok(vec![topic])
    }

    fn execute_external_command(
        &self,
        config: &Config,
        command: &str,
        args: Vec<String>,
    ) -> Result<()> {
        if !self.is_installed() {
            return Err(PluginNotInstalled(self.name.clone()).into());
        }
        let script = Script::RunExternalCommand(
            self.plugin_path
                .join("lib/commands")
                .join(format!("command-{command}.bash")),
            args,
        );
        let result = self
            .script_man
            .cmd(&config.settings, &script)
            .unchecked()
            .run()?;
        exit(result.status.code().unwrap_or(1));
    }

    fn install_version(
        &self,
        config: &Config,
        tv: &ToolVersion,
        pr: &ProgressReport,
    ) -> Result<()> {
        let run_script = |script| {
            self.script_man_for_tv(config, tv)
                .run_by_line(&config.settings, script, pr)
        };

        if self.script_man_for_tv(config, tv).script_exists(&Download) {
            pr.set_message("downloading");
            run_script(&Download)?;
        }
        pr.set_message("installing");
        run_script(&Install)?;

        Ok(())
    }

    fn uninstall_version(&self, config: &Config, tv: &ToolVersion) -> Result<()> {
        if self.plugin_path.join("bin/uninstall").exists() {
            self.script_man_for_tv(config, tv)
                .run(&config.settings, &Script::Uninstall)?;
        }
        Ok(())
    }

    fn list_bin_paths(&self, config: &Config, tv: &ToolVersion) -> Result<Vec<PathBuf>> {
        self.cache
            .list_bin_paths(config, self, tv, || self.fetch_bin_paths(config, tv))
    }

    fn exec_env(&self, config: &Config, tv: &ToolVersion) -> Result<HashMap<String, String>> {
        if matches!(tv.request, ToolVersionRequest::System(_)) {
            return Ok(EMPTY_HASH_MAP.clone());
        }
        if !self.script_man.script_exists(&ExecEnv) || *env::__RTX_SCRIPT {
            // if the script does not exist, or we're already running from within a script,
            // the second is to prevent infinite loops
            return Ok(EMPTY_HASH_MAP.clone());
        }
        self.cache
            .exec_env(config, self, tv, || self.fetch_exec_env(config, tv))
    }
}

impl Debug for ExternalPlugin {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalPlugin")
            .field("name", &self.name)
            .field("plugin_path", &self.plugin_path)
            .field("cache_path", &self.cache_path)
            .field("downloads_path", &self.downloads_path)
            .field("installs_path", &self.installs_path)
            .field("repo_url", &self.repo_url)
            .finish()
    }
}

static EMPTY_HASH_MAP: Lazy<HashMap<String, String>> = Lazy::new(HashMap::new);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug() {
        let plugin = ExternalPlugin::new(PluginName::from("dummy"));
        assert!(format!("{:?}", plugin).starts_with("ExternalPlugin { name: \"dummy\""));
    }
}
