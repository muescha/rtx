use color_eyre::eyre::{eyre, Result};
use console::style;

use crate::cli::args::tool::{ToolArg, ToolArgParser};
use crate::cli::command::Command;
use crate::config::Config;
use crate::output::Output;
use crate::toolset::{ToolVersion, ToolVersionRequest, ToolsetBuilder};
use crate::ui::multi_progress_report::MultiProgressReport;
use crate::{runtime_symlinks, shims};

/// Removes runtime versions
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, alias = "remove", alias = "rm", after_long_help = AFTER_LONG_HELP)]
pub struct Uninstall {
    /// Tool(s) to remove
    #[clap(required = true, value_name="TOOL@VERSION", value_parser = ToolArgParser)]
    tool: Vec<ToolArg>,

    /// Delete all installed versions
    #[clap(long, short = 'a')]
    all: bool,

    /// Do not actually delete anything
    #[clap(long, short = 'n')]
    dry_run: bool,
}

impl Command for Uninstall {
    fn run(self, mut config: Config, _out: &mut Output) -> Result<()> {
        let runtimes = ToolArg::double_tool_condition(&self.tool);

        let mut tool_versions = vec![];
        if self.all {
            for runtime in runtimes {
                let tool = config.get_or_create_tool(&runtime.plugin);
                let query = runtime.tvr.map(|tvr| tvr.version()).unwrap_or_default();
                let tvs = tool
                    .list_installed_versions()?
                    .into_iter()
                    .filter(|v| v.starts_with(&query))
                    .map(|v| {
                        let tvr = ToolVersionRequest::new(tool.name.clone(), &v);
                        let tv = ToolVersion::new(&tool, tvr, Default::default(), v);
                        (tool.clone(), tv)
                    })
                    .collect::<Vec<_>>();
                if tvs.is_empty() {
                    warn!(
                        "no versions found for {}",
                        style(&tool.name).cyan().for_stderr()
                    );
                }
                tool_versions.extend(tvs);
            }
        } else {
            tool_versions = runtimes
                .into_iter()
                .map(|a| {
                    let tool = config.get_or_create_tool(&a.plugin);
                    let tvs = match a.tvr {
                        Some(tvr) => {
                            vec![tvr.resolve(&config, &tool, Default::default(), false)?]
                        }
                        None => {
                            let ts = ToolsetBuilder::new().build(&mut config)?;
                            let tvl = ts.versions.get(&a.plugin).expect("no versions found");
                            tvl.versions.clone()
                        }
                    };
                    Ok(tvs
                        .into_iter()
                        .map(|tv| (tool.clone(), tv))
                        .collect::<Vec<_>>())
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        }

        let mpr = MultiProgressReport::new(config.show_progress_bars());
        for (plugin, tv) in tool_versions {
            if !plugin.is_version_installed(&tv) {
                warn!("{} is not installed", style(&tv).cyan().for_stderr());
                continue;
            }

            let mut pr = mpr.add();
            plugin.decorate_progress_bar(&mut pr, Some(&tv));
            if let Err(err) = plugin.uninstall_version(&config, &tv, &pr, self.dry_run) {
                pr.error(err.to_string());
                return Err(eyre!(err).wrap_err(format!("failed to uninstall {}", &tv)));
            }
            pr.finish_with_message("uninstalled");
        }

        let ts = ToolsetBuilder::new().build(&mut config)?;
        shims::reshim(&config, &ts).map_err(|err| eyre!("failed to reshim: {}", err))?;
        runtime_symlinks::rebuild(&config)?;

        Ok(())
    }
}

static AFTER_LONG_HELP: &str = color_print::cstr!(
    r#"<bold><underline>Examples:</underline></bold>
  $ <bold>rtx uninstall node@18.0.0</bold> # will uninstall specific version
  $ <bold>rtx uninstall node</bold>        # will uninstall current node version
  $ <bold>rtx uninstall --all node@18.0.0</bold> # will uninstall all node versions
"#
);
