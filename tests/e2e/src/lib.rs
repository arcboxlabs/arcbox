use std::path::PathBuf;

pub mod boot_assets;
pub mod daemon;
pub mod docker;
pub mod http;
pub mod metrics;
pub mod net_fixtures;
pub mod sandbox;
pub mod sandbox_bench;
pub mod scenario;
pub mod sdk_py;
pub mod sdk_ts;
pub mod signing;
pub mod virtio_debug;

pub fn repo_root() -> PathBuf {
    if let Some(root) = std::env::var_os("ARCBOX_E2E_REPO_ROOT").filter(|root| !root.is_empty()) {
        return PathBuf::from(root);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("tests/e2e lives two levels below the repository root")
        .to_owned()
}

/// True when the environment variable is set to a truthy value (or set
/// but empty), e.g. `SKIP_BUILD=1` / `KEEP_TEST_DIR=yes`.
pub fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        value.is_empty()
            || matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
    })
}

/// Retains the original failure when finalization also fails.
pub(crate) fn combine_results(
    result: anyhow::Result<()>,
    finalization: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (result, finalization) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(error), Err(finalization)) => {
            Err(error.context(format!("finalization also failed: {finalization:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::combine_results;

    #[test]
    fn finalization_errors_preserve_the_original_failure() {
        for (scenario_failed, finalization_failed) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let scenario = if scenario_failed {
                Err(std::io::Error::other("scenario failure").into())
            } else {
                Ok(())
            };
            let finalization = if finalization_failed {
                Err(anyhow::anyhow!("cleanup failure"))
            } else {
                Ok(())
            };
            let result = combine_results(scenario, finalization);
            assert_eq!(result.is_err(), scenario_failed || finalization_failed);
            if let Err(error) = result {
                let report = format!("{error:#}");
                assert_eq!(report.contains("scenario failure"), scenario_failed);
                assert_eq!(report.contains("cleanup failure"), finalization_failed);
                assert_eq!(
                    error.downcast_ref::<std::io::Error>().is_some(),
                    scenario_failed
                );
            }
        }
    }
}
