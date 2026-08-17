use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};

use crate::release_inventory::task_statuses;

#[derive(Clone, Debug, Deserialize)]
pub struct AdversarialJob {
    pub id: String,
    pub workflow: String,
    pub event: String,
    pub owner: String,
}

#[derive(Debug, Deserialize)]
struct JobFile {
    schema: u32,
    jobs: Vec<AdversarialJob>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub head_sha: String,
    pub event: String,
    pub status: String,
    pub conclusion: String,
    pub completed_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JobRun {
    pub job: String,
    pub workflow: String,
    pub event: String,
    pub latest: Option<WorkflowRun>,
    pub last_success: Option<WorkflowRun>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RunFile {
    schema: u32,
    runs: Vec<JobRun>,
}

#[derive(Debug)]
pub struct AdversarialInventory {
    pub jobs: Vec<AdversarialJob>,
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub struct AdversarialEvidence {
    pub runs: BTreeMap<String, JobRun>,
    pub errors: Vec<String>,
}

impl AdversarialInventory {
    pub fn load(root: &Path) -> Self {
        let mut errors = Vec::new();
        let jobs = match fs::read(root.join("adversarial-jobs.json"))
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<JobFile>(&bytes).map_err(|e| e.to_string()))
        {
            Ok(file) if file.schema == 2 => file.jobs,
            Ok(file) => {
                errors.push(format!(
                    "adversarial-jobs.json has unsupported schema {}",
                    file.schema
                ));
                Vec::new()
            }
            Err(error) => {
                errors.push(format!("adversarial-jobs.json is unreadable: {error}"));
                Vec::new()
            }
        };
        let statuses = task_statuses(root, &mut errors);
        let mut ids = BTreeSet::new();
        let mut workflows = BTreeSet::new();
        for job in &jobs {
            if job.id.is_empty() || !ids.insert(job.id.as_str()) {
                errors.push(format!(
                    "adversarial job {:?} is empty or duplicated",
                    job.id
                ));
            }
            if !matches!(job.event.as_str(), "push" | "schedule") {
                errors.push(format!(
                    "adversarial job {} has unsupported event {:?}",
                    job.id, job.event
                ));
            }
            let path = Path::new(&job.workflow);
            if path.components().count() != 1
                || !matches!(path.components().next(), Some(Component::Normal(_)))
                || !workflows.insert(job.workflow.as_str())
                || !root.join(".github/workflows").join(path).is_file()
            {
                errors.push(format!(
                    "adversarial job {} has invalid, duplicated or missing workflow {:?}",
                    job.id, job.workflow
                ));
            }
            match statuses.get(&job.owner).map(String::as_str) {
                Some(status) if status != "cancelled" => {}
                status => errors.push(format!(
                    "adversarial job {} has owner task {:?} with status {status:?}",
                    job.id, job.owner
                )),
            }
        }
        if jobs.is_empty() {
            errors.push("adversarial-jobs.json names no mandatory jobs".to_owned());
        }
        Self { jobs, errors }
    }

    pub fn evaluate(&self, path: &Path, revision: &str) -> AdversarialEvidence {
        let mut errors = Vec::new();
        let file = match fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<RunFile>(&bytes).map_err(|e| e.to_string()))
        {
            Ok(file) if file.schema == 2 => file,
            Ok(file) => {
                errors.push(format!(
                    "{} has unsupported schema {}",
                    path.display(),
                    file.schema
                ));
                return AdversarialEvidence {
                    runs: BTreeMap::new(),
                    errors,
                };
            }
            Err(error) => {
                errors.push(format!("{} is unreadable: {error}", path.display()));
                return AdversarialEvidence {
                    runs: BTreeMap::new(),
                    errors,
                };
            }
        };
        let mut runs = BTreeMap::new();
        for run in file.runs {
            let id = run.job.clone();
            if runs.insert(id.clone(), run).is_some() {
                errors.push(format!("adversarial run {id} is duplicated"));
            }
        }
        for job in &self.jobs {
            let Some(run) = runs.get(&job.id) else {
                errors.push(format!("adversarial job {} has no run evidence", job.id));
                continue;
            };
            if run.workflow != job.workflow {
                errors.push(format!(
                    "adversarial job {} reports workflow {:?}, expected {:?}",
                    job.id, run.workflow, job.workflow
                ));
            }
            if run.event != job.event {
                errors.push(format!(
                    "adversarial job {} reports event {:?}, expected {:?}",
                    job.id, run.event, job.event
                ));
            }
            validate_run(
                job,
                "latest completed",
                run.latest.as_ref(),
                revision,
                &mut errors,
            );
            validate_run(
                job,
                "last successful",
                run.last_success.as_ref(),
                revision,
                &mut errors,
            );
            if let (Some(latest), Some(success)) = (&run.latest, &run.last_success)
                && latest.conclusion == "success"
                && latest.id != success.id
            {
                errors.push(format!(
                    "adversarial job {} names run {} as latest success but run {} as last success",
                    job.id, latest.id, success.id
                ));
            }
        }
        let expected: BTreeSet<&str> = self.jobs.iter().map(|job| job.id.as_str()).collect();
        for id in runs.keys().filter(|id| !expected.contains(id.as_str())) {
            errors.push(format!("run evidence names unknown adversarial job {id}"));
        }
        AdversarialEvidence { runs, errors }
    }
}

fn validate_run(
    job: &AdversarialJob,
    kind: &str,
    run: Option<&WorkflowRun>,
    revision: &str,
    errors: &mut Vec<String>,
) {
    let Some(run) = run else {
        errors.push(format!("adversarial job {} has no {kind} run", job.id));
        return;
    };
    if run.id == 0
        || run.status != "completed"
        || run.conclusion != "success"
        || run.head_sha != revision
        || run.event != job.event
        || run.completed_at.is_empty()
        || run.url.is_empty()
    {
        errors.push(format!(
            "adversarial job {} {kind} run {} is event={:?} status={:?} conclusion={:?} revision={:?}",
            job.id, run.id, run.event, run.status, run.conclusion, run.head_sha
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{AdversarialInventory, JobRun, RunFile, WorkflowRun};

    fn run(id: u64, revision: &str, event: &str) -> WorkflowRun {
        WorkflowRun {
            id,
            head_sha: revision.to_owned(),
            event: event.to_owned(),
            status: "completed".to_owned(),
            conclusion: "success".to_owned(),
            completed_at: "2026-08-17T00:00:00Z".to_owned(),
            url: format!("https://example.invalid/runs/{id}"),
        }
    }

    #[test]
    fn current_revision_successes_satisfy_every_adversarial_job() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is under the workspace");
        let inventory = AdversarialInventory::load(root);
        assert!(inventory.errors.is_empty(), "{:?}", inventory.errors);
        let revision = "1".repeat(40);
        let runs = inventory
            .jobs
            .iter()
            .enumerate()
            .map(|(index, job)| {
                let observed = run(
                    u64::try_from(index + 1).expect("small index"),
                    &revision,
                    &job.event,
                );
                JobRun {
                    job: job.id.clone(),
                    workflow: job.workflow.clone(),
                    event: job.event.clone(),
                    latest: Some(observed.clone()),
                    last_success: Some(observed),
                }
            })
            .collect();
        let evidence = tempfile::NamedTempFile::new().expect("evidence file");
        fs::write(
            evidence.path(),
            serde_json::to_vec(&RunFile { schema: 2, runs }).expect("encode evidence"),
        )
        .expect("write evidence");
        assert!(
            inventory
                .evaluate(evidence.path(), &revision)
                .errors
                .is_empty()
        );
    }

    #[test]
    fn a_stale_or_failed_adversarial_run_is_rejected() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is under the workspace");
        let inventory = AdversarialInventory::load(root);
        let revision = "2".repeat(40);
        let job = inventory.jobs.first().expect("a mandatory job");
        let mut latest = run(7, &"1".repeat(40), &job.event);
        latest.conclusion = "failure".to_owned();
        let file = RunFile {
            schema: 2,
            runs: vec![JobRun {
                job: job.id.clone(),
                workflow: job.workflow.clone(),
                event: job.event.clone(),
                latest: Some(latest),
                last_success: Some(run(6, &"1".repeat(40), &job.event)),
            }],
        };
        let evidence = tempfile::NamedTempFile::new().expect("evidence file");
        fs::write(
            evidence.path(),
            serde_json::to_vec(&file).expect("encode evidence"),
        )
        .expect("write evidence");
        let evaluated = inventory.evaluate(evidence.path(), &revision);
        assert!(
            evaluated
                .errors
                .iter()
                .any(|error| error.contains("status=\"completed\" conclusion=\"failure\""))
        );
        assert!(
            evaluated
                .errors
                .iter()
                .any(|error| error.contains("has no run evidence"))
        );
    }

    #[test]
    fn a_manual_fuzz_run_is_not_scheduled_evidence() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask is under the workspace");
        let inventory = AdversarialInventory::load(root);
        let revision = "3".repeat(40);
        let job = inventory
            .jobs
            .iter()
            .find(|job| job.id == "continuous-fuzz")
            .expect("continuous fuzz job");
        let manual = run(8, &revision, "workflow_dispatch");
        let file = RunFile {
            schema: 2,
            runs: vec![JobRun {
                job: job.id.clone(),
                workflow: job.workflow.clone(),
                event: job.event.clone(),
                latest: Some(manual.clone()),
                last_success: Some(manual),
            }],
        };
        let evidence = tempfile::NamedTempFile::new().expect("evidence file");
        fs::write(
            evidence.path(),
            serde_json::to_vec(&file).expect("encode evidence"),
        )
        .expect("write evidence");
        let evaluated = inventory.evaluate(evidence.path(), &revision);
        assert!(
            evaluated
                .errors
                .iter()
                .any(|error| error.contains("event=\"workflow_dispatch\""))
        );
    }
}
