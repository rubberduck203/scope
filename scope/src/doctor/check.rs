use super::file_cache::{FileCache, FileCacheStatus};
use anyhow::Result;
use std::cmp;
use std::cmp::max;
use tracing::debug;

use crate::models::HelpMetadata;
use crate::prelude::{generate_env_vars, ActionReport, ActionReportBuilder, ActionTaskReport};
use crate::shared::prelude::{
    CaptureError, CaptureOpts, DoctorCommand, DoctorGroup, DoctorGroupAction, DoctorGroupCachePath,
    ExecutionProvider, OutputDestination,
};
use async_trait::async_trait;
use derive_builder::Builder;
use educe::Educe;
use mockall::automock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{error, info, instrument};

#[allow(clippy::enum_variant_names)]
#[derive(Error, Debug)]
pub enum RuntimeError {
    #[error("Unable to process file. {error:?}")]
    IoError {
        #[from]
        error: std::io::Error,
    },
    #[error("Unable to parse UTF-8 output. {error:?}")]
    FromUtf8Error {
        #[from]
        error: std::string::FromUtf8Error,
    },
    #[error(transparent)]
    CaptureError(#[from] CaptureError),
    #[error(transparent)]
    AnyError(#[from] anyhow::Error),
    #[error(transparent)]
    PatternError(#[from] glob::PatternError),
}

#[derive(Debug, Clone, PartialEq, Ord, Eq, PartialOrd)]
pub enum CacheStatus {
    FixNotRequired = 1,
    FixRequired = 2,
    StopExecution = 3,
    CacheNotDefined = 4,
}

impl CacheStatus {
    fn is_success(&self) -> bool {
        self == &CacheStatus::FixNotRequired || self == &CacheStatus::CacheNotDefined
    }
}

#[derive(Debug, Clone)]
pub struct CacheResults {
    pub status: CacheStatus,
    pub output: Option<Vec<ActionTaskReport>>,
}

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum ActionRunStatus {
    CheckSucceeded,
    CheckFailedFixSucceedVerifySucceed,
    CheckFailedFixFailed,
    CheckFailedFixSucceedVerifyFailed,
    CheckFailedNoRunFix,
    CheckFailedNoFixProvided,
    CheckFailedFixFailedStop,
    CheckFailedFixUserDenied,
    NoCheckFixSucceeded,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActionRunResult {
    pub action_name: String,
    pub status: ActionRunStatus,
    pub action_report: ActionReport,
}

impl ActionRunResult {
    pub fn new(
        name: &str,
        status: ActionRunStatus,
        check_output: Option<Vec<ActionTaskReport>>,
        fix_output: Option<Vec<ActionTaskReport>>,
        validate_output: Option<Vec<ActionTaskReport>>,
    ) -> Self {
        let mut builder = ActionReportBuilder::default();
        builder.action_name(name);

        if let Some(output) = check_output {
            builder.check(output);
        }
        if let Some(output) = fix_output {
            builder.fix(output);
        }
        if let Some(output) = validate_output {
            builder.validate(output);
        }

        Self {
            action_name: name.to_string(),
            status,
            action_report: builder
                .build()
                .expect("report builder to have all values set"),
        }
    }
}

impl ActionRunStatus {
    pub(crate) fn is_failure(&self) -> bool {
        match self {
            ActionRunStatus::CheckSucceeded => false,
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed => false,
            ActionRunStatus::CheckFailedFixFailed => true,
            ActionRunStatus::CheckFailedFixSucceedVerifyFailed => true,
            ActionRunStatus::CheckFailedNoRunFix => true,
            ActionRunStatus::CheckFailedNoFixProvided => true,
            ActionRunStatus::CheckFailedFixFailedStop => true,
            ActionRunStatus::CheckFailedFixUserDenied => false,
            ActionRunStatus::NoCheckFixSucceeded => false,
        }
    }
}

#[automock]
#[async_trait::async_trait]
pub trait DoctorActionRun: Send + Sync {
    async fn run_action(
        &self,
        prompt: for<'a> fn(&'a str, &'a Option<String>) -> bool,
    ) -> Result<ActionRunResult>;
    fn required(&self) -> bool;
    fn name(&self) -> String;
    fn description(&self) -> String;
    fn help_text(&self) -> Option<String>;
    fn help_url(&self) -> Option<String>;
}

#[derive(Educe, Builder)]
#[educe(Debug)]
#[builder(setter(into))]
pub struct DefaultDoctorActionRun {
    pub model: DoctorGroup,
    pub action: DoctorGroupAction,
    pub working_dir: PathBuf,
    pub file_cache: Arc<dyn FileCache>,
    pub run_fix: bool,
    #[educe(Debug(ignore))]
    pub exec_runner: Arc<dyn ExecutionProvider>,
    #[educe(Debug(ignore))]
    pub glob_walker: Arc<dyn GlobWalker>,
}

const NO_COMMANDS_EXIT_CODE: i32 = -1;
const USER_DENIED_EXIT_CODE: i32 = -2;
//TODO: 100 is used a few times and needs a name but I don't understand why 100 well enough to name it

#[async_trait::async_trait]
impl DoctorActionRun for DefaultDoctorActionRun {
    #[instrument(skip_all, fields(model.name = self.model.name(), action.name = self.action.name, action.description = self.action.description ))]
    async fn run_action(
        &self,
        prompt: for<'a> fn(&'a str, &'a Option<String>) -> bool,
    ) -> Result<ActionRunResult> {
        let check_results = self.evaluate_checks().await?;
        let check_status = check_results.status;
        if check_status == CacheStatus::FixNotRequired {
            return Ok(ActionRunResult::new(
                &self.name(),
                ActionRunStatus::CheckSucceeded,
                check_results.output,
                None,
                None,
            ));
        }

        if !self.run_fix {
            return Ok(ActionRunResult::new(
                &self.name(),
                ActionRunStatus::CheckFailedNoRunFix,
                check_results.output,
                None,
                None,
            ));
        }

        let (fix_result, fix_output) = self.run_fixes(prompt).await?;

        match fix_result {
            i32::MIN..=-3 | NO_COMMANDS_EXIT_CODE => {
                return Ok(ActionRunResult::new(
                    &self.name(),
                    ActionRunStatus::CheckFailedNoFixProvided,
                    check_results.output,
                    None,
                    None,
                ));
            }
            USER_DENIED_EXIT_CODE => {
                return Ok(ActionRunResult::new(
                    &self.name(),
                    ActionRunStatus::CheckFailedFixUserDenied,
                    check_results.output,
                    Some(fix_output),
                    None,
                ));
            }
            0 => {}
            1...100 => {
                return Ok(ActionRunResult::new(
                    &self.name(),
                    ActionRunStatus::CheckFailedFixFailed,
                    check_results.output,
                    Some(fix_output),
                    None,
                ));
            }
            _ => {
                return Ok(ActionRunResult::new(
                    &self.name(),
                    ActionRunStatus::CheckFailedFixFailedStop,
                    check_results.output,
                    Some(fix_output),
                    None,
                ));
            }
        }

        if check_status == CacheStatus::CacheNotDefined {
            self.update_caches().await;
            return Ok(ActionRunResult::new(
                &self.name(),
                ActionRunStatus::NoCheckFixSucceeded,
                check_results.output,
                Some(fix_output),
                None,
            ));
        }

        let mut validate_output = None;
        if let Some(validate_result) = self.evaluate_command_checks().await? {
            validate_output = validate_result.output;
            if validate_result.status != CacheStatus::FixNotRequired {
                return Ok(ActionRunResult::new(
                    &self.name(),
                    ActionRunStatus::CheckFailedFixSucceedVerifyFailed,
                    check_results.output,
                    Some(fix_output),
                    validate_output,
                ));
            }
        }

        self.update_caches().await;

        return Ok(ActionRunResult::new(
            &self.name(),
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed,
            check_results.output,
            Some(fix_output),
            validate_output,
        ));
    }

    fn required(&self) -> bool {
        self.action.required
    }

    fn name(&self) -> String {
        self.action.name.to_string()
    }

    fn description(&self) -> String {
        self.action.description.to_string()
    }

    fn help_text(&self) -> Option<String> {
        self.action.fix.help_text.clone()
    }

    fn help_url(&self) -> Option<String> {
        self.action.fix.help_url.clone()
    }
}

impl DefaultDoctorActionRun {
    async fn update_caches(&self) {
        if let Some(cache_path) = &self.action.check.files {
            let result = self
                .glob_walker
                .update_cache(
                    &cache_path.base_path,
                    &cache_path.paths,
                    &self.model.metadata.name(),
                    self.file_cache.clone(),
                )
                .await;

            if let Err(e) = result {
                info!("Unable to update cache, dropping update {:?}", e);
                info!(target: "user", "Unable to update file cache, next run will re-run this action.")
            }
        }
    }

    async fn run_fixes(
        &self,
        prompt: for<'a> fn(&'a str, &'a Option<String>) -> bool,
    ) -> Result<(i32, Vec<ActionTaskReport>), RuntimeError> {
        match &self.action.fix.command {
            None => Ok((NO_COMMANDS_EXIT_CODE, Vec::new())),
            Some(action_command) => match &self.action.fix.prompt {
                None => Ok(self.run_commands(action_command).await?),
                Some(fix_prompt) => {
                    if prompt(&fix_prompt.text, &fix_prompt.extra_context) {
                        Ok(self.run_commands(action_command).await?)
                    } else {
                        Ok((USER_DENIED_EXIT_CODE, Vec::new()))
                    }
                }
            },
        }
    }

    async fn run_commands(
        &self,
        commands: &DoctorCommand,
    ) -> Result<(i32, Vec<ActionTaskReport>), RuntimeError> {
        let mut action_reports = Vec::new();
        let mut highest_exit_code = NO_COMMANDS_EXIT_CODE;

        for command in commands.expand() {
            let report = self.run_single_fix(&command).await?;
            highest_exit_code = max(
                highest_exit_code,
                report.exit_code.unwrap_or(NO_COMMANDS_EXIT_CODE),
            );
            action_reports.push(report);
            if highest_exit_code >= 100 {
                return Ok((highest_exit_code, action_reports));
            }
        }

        Ok((highest_exit_code, action_reports))
    }

    async fn run_single_fix(&self, command: &str) -> Result<ActionTaskReport, RuntimeError> {
        let args = vec![command.to_string()];
        let capture = self
            .exec_runner
            .run_command(CaptureOpts {
                working_dir: &self.working_dir,
                args: &args,
                output_dest: OutputDestination::StandardOutWithPrefix(format!(
                    "{}/{}",
                    self.model.metadata.name(),
                    self.action.name
                )),
                path: &self.model.metadata.exec_path(),
                env_vars: generate_env_vars(),
            })
            .await?;

        info!("fix ran {} and exited {:?}", command, capture.exit_code);

        Ok(ActionTaskReport::from(&capture))
    }

    async fn evaluate_checks(&self) -> Result<CacheResults, RuntimeError> {
        let (path_check, command_check) =
            match (&self.action.check.files, &self.action.check.command) {
                (None, None) => (None, None),
                (Some(cache_path), None) => {
                    let result = self.evaluate_path_check(cache_path).await?;
                    (Some(result), None)
                }
                (None, Some(check_command)) => {
                    let result = self.run_check_command(check_command).await?;
                    (None, Some(result))
                }
                (Some(cache_path), Some(check_command)) => {
                    let path_result = self.evaluate_path_check(cache_path).await?;
                    if !path_result.is_success() {
                        let result = self.run_check_command(check_command).await?;
                        (Some(path_result), Some(result))
                    } else {
                        (Some(path_result), None)
                    }
                }
            };

        let status = match (&path_check, &command_check) {
            (None, None) => CacheStatus::CacheNotDefined,
            (Some(p), None) if p.is_success() => CacheStatus::FixNotRequired,
            (None, Some(c)) if c.status.is_success() => CacheStatus::FixNotRequired,
            // If we have both path and command, that means the path check failed
            // but we only want to run the fix if the command check also failed.
            (Some(_p), Some(c)) if c.status.is_success() => CacheStatus::FixNotRequired,
            _ => CacheStatus::FixRequired,
        };

        let output = match command_check {
            Some(c) => c.output.clone(),
            None => None,
        };

        Ok(CacheResults { status, output })
    }

    async fn evaluate_command_checks(&self) -> Result<Option<CacheResults>, RuntimeError> {
        if let Some(action_command) = &self.action.check.command {
            let result = self.run_check_command(action_command).await?;
            return Ok(Some(result));
        }

        Ok(None)
    }

    async fn evaluate_path_check(
        &self,
        paths: &DoctorGroupCachePath,
    ) -> Result<CacheStatus, RuntimeError> {
        let result = self
            .glob_walker
            .have_globs_changed(
                &paths.base_path,
                &paths.paths,
                &self.model.metadata.name(),
                self.file_cache.clone(),
            )
            .await?;

        if result {
            Ok(CacheStatus::FixNotRequired)
        } else {
            Ok(CacheStatus::FixRequired)
        }
    }

    async fn run_check_command(
        &self,
        action_command: &DoctorCommand,
    ) -> Result<CacheResults, RuntimeError> {
        info!("Evaluating {:?}", action_command);
        let mut action_reports = Vec::new();
        let mut result: Option<CacheStatus> = None;

        for command in &action_command.expand() {
            let args = vec![command.clone()];
            let path = format!(
                "{}:{}",
                self.model.metadata().containing_dir(),
                self.model.metadata().exec_path()
            );
            let output = self
                .exec_runner
                .run_command(CaptureOpts {
                    working_dir: &self.working_dir,
                    args: &args,
                    output_dest: OutputDestination::Logging,
                    path: &path,
                    env_vars: generate_env_vars(),
                })
                .await?;

            action_reports.push(ActionTaskReport::from(&output));

            info!(
                "check ran command {} and result was {:?}",
                command, output.exit_code
            );

            let command_result = match output.exit_code {
                Some(0) => CacheStatus::FixNotRequired,
                Some(100..=i32::MAX) => CacheStatus::StopExecution,
                _ => CacheStatus::FixRequired,
            };

            let next = match &result {
                None => command_result,
                Some(prev) => cmp::max(prev.clone(), command_result.clone()),
            };

            result.replace(next);
            if result == Some(CacheStatus::StopExecution) {
                break;
            }
        }

        let status = result.unwrap_or(CacheStatus::FixRequired);
        Ok(CacheResults {
            status,
            output: Some(action_reports),
        })
    }
}

#[automock]
#[async_trait]
pub trait GlobWalker: Send + Sync {
    async fn have_globs_changed(
        &self,
        base_dir: &Path,
        paths: &[String],
        cache_name: &str,
        file_cache: Arc<dyn FileCache>,
    ) -> Result<bool, RuntimeError>;

    async fn update_cache(
        &self,
        base_dir: &Path,
        paths: &[String],
        cache_name: &str,
        file_cache: Arc<dyn FileCache>,
    ) -> Result<(), RuntimeError>;
}

#[automock]
trait FileSystem: Send + Sync {
    fn find_files(&self, glob_pattern: &str) -> Result<Vec<PathBuf>>;
}

#[derive(Debug, Default)]
struct DefaultFileSystem {}

/// Abstract away filesystem access for use in testing.
/// This trait should be a thin wrapper around actions to the filesystem, ideally just action
/// and error handling. Adding more logic will make testing impossible without setting up a
/// filesystem.
impl FileSystem for DefaultFileSystem {
    /// Search for a glob pattern. This function expects the path to be absolute already,
    /// so that it's not dependent on the working directory.
    fn find_files(&self, glob_pattern: &str) -> Result<Vec<PathBuf>> {
        Ok(glob::glob(glob_pattern)?.filter_map(Result::ok).collect())
    }
}

#[derive(Educe)]
#[educe(Debug)]
pub struct DefaultGlobWalker {
    #[educe(Debug(ignore))]
    file_system: Box<dyn FileSystem>,
}

impl Default for DefaultGlobWalker {
    fn default() -> Self {
        Self {
            file_system: Box::<DefaultFileSystem>::default(),
        }
    }
}

fn make_absolute(base_dir: &Path, glob: &String) -> String {
    let expanded_glob = shellexpand::tilde(glob);
    let glob_buf = PathBuf::from(expanded_glob.to_string());
    if glob_buf.is_absolute() {
        glob_buf.display().to_string()
    } else {
        base_dir.join(glob_buf).display().to_string()
    }
}

#[async_trait]
impl GlobWalker for DefaultGlobWalker {
    async fn have_globs_changed(
        &self,
        base_dir: &Path,
        paths: &[String],
        cache_name: &str,
        file_cache: Arc<dyn FileCache>,
    ) -> Result<bool, RuntimeError> {
        for glob_str in paths {
            let glob_path = make_absolute(base_dir, glob_str);
            let files = self.file_system.find_files(&glob_path)?;

            if files.is_empty() {
                return Ok(false);
            }

            for path in files {
                let file_result = file_cache.check_file(cache_name.to_string(), &path).await?;
                debug!(target: "user", "CacheStatus for file {}: {:?}", path.display(), file_result);
                let check_result = file_result == FileCacheStatus::FileMatches;
                if !check_result {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    async fn update_cache(
        &self,
        base_dir: &Path,
        paths: &[String],
        cache_name: &str,
        file_cache: Arc<dyn FileCache>,
    ) -> Result<(), RuntimeError> {
        for glob_str in paths {
            let glob_path = make_absolute(base_dir, glob_str);
            for path in self.file_system.find_files(&glob_path)? {
                file_cache
                    .update_cache_entry(cache_name.to_string(), &path)
                    .await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::doctor::check::{
        ActionRunStatus, DefaultDoctorActionRun, DefaultGlobWalker, DoctorActionRun, GlobWalker,
        MockFileSystem, MockGlobWalker, RuntimeError,
    };
    use crate::doctor::file_cache::{FileCache, MockFileCache, NoOpCache};
    use crate::doctor::tests::build_root_model;
    use crate::shared::prelude::*;
    use anyhow::{anyhow, Result};
    use directories::UserDirs;
    use predicates::prelude::predicate;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    pub fn build_run_fail_fix_succeed_action() -> DoctorGroupAction {
        DoctorGroupActionBuilder::default()
            .description("a test action")
            .name("action")
            .required(true)
            .check(
                DoctorGroupActionCheckBuilder::default()
                    .files(None)
                    .command(Some(DoctorCommand::from(vec!["check"])))
                    .build()
                    .unwrap(),
            )
            .fix(
                DoctorFixBuilder::default()
                    .command(Some(DoctorCommand::from(vec!["fix"])))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    pub fn build_run_action_that_prompts_user() -> DoctorGroupAction {
        DoctorGroupActionBuilder::default()
            .description("a test action that prompts the user before applying fix")
            .name("action")
            .required(true)
            .check(
                DoctorGroupActionCheckBuilder::default()
                    .files(None)
                    .command(Some(DoctorCommand::from(vec!["check"])))
                    .build()
                    .unwrap(),
            )
            .fix(
                DoctorFixBuilder::default()
                    .command(Some(DoctorCommand::from(vec!["fix"])))
                    .prompt(Some(DoctorFixPrompt {
                        text: "do you want to continue?".to_string(),
                        extra_context: Some("additional context here".to_string()),
                    }))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    pub fn build_file_fix_action() -> DoctorGroupAction {
        DoctorGroupActionBuilder::default()
            .description("a test action")
            .name("action")
            .required(true)
            .check(
                DoctorGroupActionCheckBuilder::default()
                    .command(None)
                    .files(Some(DoctorGroupCachePath::from(("/foo", vec!["**/*"]))))
                    .build()
                    .unwrap(),
            )
            .fix(
                DoctorFixBuilder::default()
                    .command(Some(DoctorCommand::from(vec!["fix"])))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    pub fn command_result(
        mock: &mut MockExecutionProvider,
        command: &'static str,
        expected_results: Vec<i32>,
    ) {
        let mut counter = 0;
        mock.expect_run_command()
            .times(expected_results.len())
            .withf(move |params| {
                params.args[0].eq(command) && params.env_vars.contains_key("SCOPE_BIN_DIR")
            })
            .returning(move |capture_opts| {
                let resp_code = expected_results[counter];
                counter += 1;
                Ok(OutputCaptureBuilder::default()
                    .command(capture_opts.args[0].to_string())
                    .exit_code(Some(resp_code))
                    .build()
                    .unwrap())
            });
    }

    pub fn setup_test(
        actions: Vec<DoctorGroupAction>,
        exec_runner: MockExecutionProvider,
        glob_walker: MockGlobWalker,
    ) -> DefaultDoctorActionRun {
        let model = build_root_model(actions.clone());
        let path = PathBuf::from("/tmp/foo");
        let file_cache: Arc<dyn FileCache> = Arc::<NoOpCache>::default();

        DefaultDoctorActionRun {
            model,
            action: actions[0].clone(),
            working_dir: path,
            file_cache,
            run_fix: true,
            exec_runner: Arc::new(exec_runner),
            glob_walker: Arc::new(glob_walker),
        }
    }

    fn panic_if_user_is_prompted(_: &str, _: &Option<String>) -> bool {
        unimplemented!("must not prompt user")
    }

    fn user_responds_yes(_: &str, _: &Option<String>) -> bool {
        true
    }

    fn user_responds_no(_: &str, _: &Option<String>) -> bool {
        false
    }

    fn home_dir() -> PathBuf {
        let user_dirs = UserDirs::new().expect("Couldn't initialize UserDirs");
        user_dirs.home_dir().to_owned()
    }

    #[tokio::test]
    async fn test_only_exec_will_check_passes() -> Result<()> {
        let action = build_run_fail_fix_succeed_action();
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        command_result(&mut exec_runner, "check", vec![0]);

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(ActionRunStatus::CheckSucceeded, result.status);
        assert!(result.action_report.check.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_only_exec_will_re_run() -> Result<()> {
        let action = build_run_fail_fix_succeed_action();
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        command_result(&mut exec_runner, "check", vec![1, 0]);
        command_result(&mut exec_runner, "fix", vec![0]);

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed,
            result.status
        );

        assert!(result.action_report.check.len() == 1);
        assert!(result.action_report.fix.len() == 1);
        assert!(result.action_report.validate.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_fail_fix_succeed_check_fails() -> Result<()> {
        let action = build_run_fail_fix_succeed_action();
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        command_result(&mut exec_runner, "check", vec![1, 1]);
        command_result(&mut exec_runner, "fix", vec![0]);

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(
            ActionRunStatus::CheckFailedFixSucceedVerifyFailed,
            result.status
        );

        assert!(result.action_report.check.len() == 1);
        assert!(result.action_report.fix.len() == 1);
        assert!(result.action_report.validate.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_prompt_user_says_yes_fix_runs() -> Result<()> {
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        let action = build_run_action_that_prompts_user();

        command_result(&mut exec_runner, "check", vec![1, 0]);
        command_result(&mut exec_runner, "fix", vec![0]);

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(user_responds_yes).await?;
        assert_eq!(
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed,
            result.status
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_prompt_user_says_no_fix_does_not_run() -> Result<()> {
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        let action = build_run_action_that_prompts_user();

        command_result(&mut exec_runner, "check", vec![1]);
        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(user_responds_no).await?;
        assert_eq!(ActionRunStatus::CheckFailedFixUserDenied, result.status);

        Ok(())
    }

    #[tokio::test]
    async fn test_fail_fix_fails() -> Result<()> {
        let action = build_run_fail_fix_succeed_action();
        let mut exec_runner = MockExecutionProvider::new();
        let glob_walker = MockGlobWalker::new();

        command_result(&mut exec_runner, "check", vec![1]);
        command_result(&mut exec_runner, "fix", vec![1]);

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(ActionRunStatus::CheckFailedFixFailed, result.status);

        assert!(result.action_report.check.len() == 1);
        assert!(result.action_report.fix.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_cache_invalid_fix_works() -> Result<()> {
        let action = build_file_fix_action();
        let mut glob_walker = MockGlobWalker::new();
        let mut exec_runner = MockExecutionProvider::new();

        command_result(&mut exec_runner, "fix", vec![0]);

        glob_walker
            .expect_have_globs_changed()
            .times(1)
            .returning(|_, _, _, _| Ok(false));
        glob_walker
            .expect_update_cache()
            .times(1)
            .returning(|_, _, _, _| Ok(()));

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed,
            result.status
        );

        assert!(result.action_report.fix.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_cache_invalid_fix_works_unable_to_update_cache() -> Result<()> {
        let action = build_file_fix_action();
        let mut glob_walker = MockGlobWalker::new();
        let mut exec_runner = MockExecutionProvider::new();

        command_result(&mut exec_runner, "fix", vec![0]);

        glob_walker
            .expect_have_globs_changed()
            .times(1)
            .returning(|_, _, _, _| Ok(false));
        glob_walker
            .expect_update_cache()
            .times(1)
            .returning(|_, _, _, _| Err(RuntimeError::AnyError(anyhow!("bogus error"))));

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(
            ActionRunStatus::CheckFailedFixSucceedVerifySucceed,
            result.status
        );

        assert!(result.action_report.fix.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_cache_invalid_fix_fails() -> Result<()> {
        let action = build_file_fix_action();
        let mut exec_runner = MockExecutionProvider::new();
        let mut glob_walker = MockGlobWalker::new();

        command_result(&mut exec_runner, "fix", vec![1]);

        glob_walker
            .expect_have_globs_changed()
            .times(1)
            .returning(|_, _, _, _| Ok(false));
        glob_walker.expect_update_cache().never();

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(ActionRunStatus::CheckFailedFixFailed, result.status);

        assert!(result.action_report.fix.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_cache_valid_check_command_does_not_run() -> Result<()> {
        let check_command = "some_command";
        let action = DoctorGroupActionBuilder::default()
            .description("a test action")
            .name("action")
            .required(true)
            .check(
                DoctorGroupActionCheckBuilder::default()
                    .command(Some(DoctorCommand {
                        commands: vec![check_command.to_string()],
                    }))
                    .files(Some(DoctorGroupCachePath::from(("/foo", vec!["**/*"]))))
                    .build()
                    .unwrap(),
            )
            .fix(
                DoctorFixBuilder::default()
                    .command(Some(DoctorCommand::from(vec!["fix"])))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let mut exec_runner = MockExecutionProvider::new();
        let mut glob_walker = MockGlobWalker::new();

        glob_walker
            .expect_have_globs_changed()
            .times(1)
            .returning(|_, _, _, _| Ok(true));
        glob_walker.expect_update_cache().never();

        exec_runner
            .expect_run_command()
            .withf(move |params| params.args[0].eq(check_command))
            .never()
            .returning(move |capture_opts| {
                Ok(OutputCaptureBuilder::default()
                    .command(capture_opts.args[0].to_string())
                    .exit_code(Some(0))
                    .build()
                    .unwrap())
            });

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(ActionRunStatus::CheckSucceeded, result.status);

        assert!(result.action_report.check.is_empty());
        assert!(result.action_report.fix.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_file_cache_invalid_check_command_valid() -> Result<()> {
        let check_command = "some_command";
        let action = DoctorGroupActionBuilder::default()
            .description("a test action")
            .name("action")
            .required(true)
            .check(
                DoctorGroupActionCheckBuilder::default()
                    .command(Some(DoctorCommand {
                        commands: vec![check_command.to_string()],
                    }))
                    .files(Some(DoctorGroupCachePath::from(("/foo", vec!["**/*"]))))
                    .build()
                    .unwrap(),
            )
            .fix(
                DoctorFixBuilder::default()
                    .command(Some(DoctorCommand::from(vec!["fix"])))
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let mut exec_runner = MockExecutionProvider::new();
        let mut glob_walker = MockGlobWalker::new();

        glob_walker
            .expect_have_globs_changed()
            .times(1)
            .returning(|_, _, _, _| Ok(false));
        glob_walker.expect_update_cache().never();

        exec_runner
            .expect_run_command()
            .withf(move |params| params.args[0].eq(check_command))
            .once()
            .returning(move |capture_opts| {
                Ok(OutputCaptureBuilder::default()
                    .command(capture_opts.args[0].to_string())
                    .exit_code(Some(0))
                    .build()
                    .unwrap())
            });

        let run = setup_test(vec![action], exec_runner, glob_walker);

        let result = run.run_action(panic_if_user_is_prompted).await?;
        assert_eq!(ActionRunStatus::CheckSucceeded, result.status);

        assert!(result.action_report.check.len() == 1);
        assert!(result.action_report.fix.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_glob_walker_update_path_will_add_base_dir_to_path() {
        let mut file_system = MockFileSystem::new();
        let mut file_cache = MockFileCache::new();

        file_cache
            .expect_update_cache_entry()
            .once()
            .with(
                predicate::eq("file_cache".to_string()),
                predicate::eq(Path::new("/foo/bar")),
            )
            .returning(|_, _| Ok(()));

        file_system
            .expect_find_files()
            .once()
            .with(predicate::eq("/foo/root/*.txt"))
            .returning(|_| Ok(vec![PathBuf::from("/foo/bar")]));

        let walker = DefaultGlobWalker {
            file_system: Box::new(file_system),
        };

        let res = walker
            .update_cache(
                Path::new("/foo/root"),
                &["*.txt".to_string()],
                "file_cache",
                Arc::new(file_cache),
            )
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_glob_walker_update_path_honor_abs_paths() {
        let mut file_system = MockFileSystem::new();
        let mut file_cache = MockFileCache::new();

        file_cache
            .expect_update_cache_entry()
            .once()
            .with(
                predicate::eq("file_cache".to_string()),
                predicate::eq(Path::new("/foo/bar")),
            )
            .returning(|_, _| Ok(()));

        file_system
            .expect_find_files()
            .once()
            .with(predicate::eq("/a/abs/path/*.txt"))
            .returning(|_| Ok(vec![PathBuf::from("/foo/bar")]));

        let walker = DefaultGlobWalker {
            file_system: Box::new(file_system),
        };

        let res = walker
            .update_cache(
                Path::new("/foo/root"),
                &["/a/abs/path/*.txt".to_string()],
                "file_cache",
                Arc::new(file_cache),
            )
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_glob_walker_update_path_honors_tilde_paths() {
        let mut file_system = MockFileSystem::new();
        let mut file_cache = MockFileCache::new();

        let home_dir = home_dir();
        let resolved_path = home_dir.join("path/foo.txt").as_path().to_owned();

        file_cache
            .expect_update_cache_entry()
            .once()
            .with(
                predicate::eq("group_name".to_string()),
                predicate::eq(resolved_path.clone()),
            )
            .returning(|_, _| Ok(()));

        file_system
            .expect_find_files()
            .once()
            .withf(move |glob_pattern| {
                home_dir
                    .join("path/*.txt")
                    .as_path()
                    .to_owned()
                    .to_str()
                    .unwrap()
                    == glob_pattern
            })
            .returning(move |_| Ok(vec![resolved_path.clone()]));

        let walker = DefaultGlobWalker {
            file_system: Box::new(file_system),
        };

        let res = walker
            .update_cache(
                Path::new("/foo/root"),
                &["~/path/*.txt".to_string()],
                "group_name",
                Arc::new(file_cache),
            )
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_glob_walker_update_path_honors_relative_paths() {
        // I can make an argument that we should toss this test
        // and say that the correct thing to do is use **/path/*.txt
        let mut file_system = MockFileSystem::new();
        let mut file_cache = MockFileCache::new();

        file_cache
            .expect_update_cache_entry()
            .once()
            .with(
                predicate::eq("group_name".to_string()),
                predicate::eq(Path::new("/foo/path/foo.txt")),
            )
            .returning(|_, _| Ok(()));

        file_system
            .expect_find_files()
            .once()
            //glob() will error on relative paths! This is wrong!
            .with(predicate::eq("/foo/root/../path/*.txt"))
            // this is the correct expectation
            // .with(predicate::eq("/foo/path/*.txt"))
            .returning(|_| Ok(vec![PathBuf::from("/foo/path/foo.txt")]));

        let walker = DefaultGlobWalker {
            file_system: Box::new(file_system),
        };

        let res = walker
            .update_cache(
                Path::new("/foo/root"),
                &["../path/*.txt".to_string()],
                "group_name",
                Arc::new(file_cache),
            )
            .await;
        assert!(res.is_ok());
    }

    mod test_run_commands {
        use super::*;

        #[tokio::test]
        async fn expands_tilde_to_home_dir() {
            let mut exec_runner = MockExecutionProvider::new();
            exec_runner
                .expect_run_command()
                .once()
                .returning(move |capture_opts| {
                    Ok(OutputCaptureBuilder::default()
                        .command(capture_opts.args[0].to_string())
                        .exit_code(Some(0))
                        .build()
                        .unwrap())
                });

            let run = setup_test(
                vec![build_run_fail_fix_succeed_action()],
                exec_runner,
                MockGlobWalker::new(),
            );

            let (_highest_status_code, reports) = run
                .run_commands(&DoctorCommand {
                    commands: vec!["touch ~/.somefile".to_string()],
                })
                .await
                .unwrap();
            let report = reports[0].clone();
            let actual_command = report.command;

            assert_eq!(
                format!("{} {}", "touch", home_dir().join(".somefile").display()),
                actual_command
            );
        }
    }

    mod test_run_check_command {
        use super::*;

        #[tokio::test]
        async fn expands_tilde_to_home_dir() {
            let mut exec_runner = MockExecutionProvider::new();
            exec_runner
                .expect_run_command()
                .once()
                .returning(move |capture_opts| {
                    Ok(OutputCaptureBuilder::default()
                        .command(capture_opts.args[0].to_string())
                        .exit_code(Some(0))
                        .build()
                        .unwrap())
                });

            let run = setup_test(
                vec![build_run_fail_fix_succeed_action()],
                exec_runner,
                MockGlobWalker::new(),
            );

            let action_commands = DoctorCommandBuilder::default()
                .commands(vec!["test -f ~/.somefile".to_string()])
                .build()
                .unwrap();

            let result = run.run_check_command(&action_commands).await.unwrap();
            let reports = result.output.expect("Expected ActionTaskreports");
            assert_eq!(
                format!("{} {}", "test -f", home_dir().join(".somefile").display()),
                reports[0].command
            );
        }
    }

    mod test_make_absolute {
        use super::*;
        use crate::doctor::check::make_absolute;

        #[test]
        fn filename_gets_preprended_with_basepath() {
            let base_dir = Path::new("/Users/first.last/path/to/project");
            let glob = "foo.txt".to_string();

            let actual = make_absolute(base_dir, &glob);
            assert_eq!("/Users/first.last/path/to/project/foo.txt", &actual)
        }

        #[test]
        fn wildcard_does_not_get_replaced() {
            let base_dir = Path::new("/Users/first.last/path/to/project");
            let glob = "*.txt".to_string();

            let actual = make_absolute(base_dir, &glob);
            assert_eq!("/Users/first.last/path/to/project/*.txt", &actual)
        }

        #[test]
        fn path_from_root_does_not_get_replaced() {
            let base_dir = Path::new("/Users/first.last/path/to/project");
            let glob = "/etc/project/foo.txt".to_string();

            let actual = make_absolute(base_dir, &glob);
            assert_eq!("/etc/project/foo.txt", &actual)
        }

        #[test]
        fn relative_paths_are_not_resolved() {
            let base_dir = Path::new("/Users/first.last/path/to/project");
            let glob = "../foo.txt".to_string();

            let actual = make_absolute(base_dir, &glob);
            assert_eq!("/Users/first.last/path/to/project/../foo.txt", &actual)
        }

        #[test]
        fn tilde_resolves() {
            let home_dir = home_dir();
            let base_dir = home_dir.join("path/to/project");
            let glob = "~/foo.txt".to_string();

            let actual = make_absolute(base_dir.as_path(), &glob);
            assert_eq!(home_dir.join("foo.txt").display().to_string(), actual);
        }
    }
}
