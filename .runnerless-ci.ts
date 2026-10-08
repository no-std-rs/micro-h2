import {
  workflow, configuration, event, repo, packages, checks, jobs, reviews,
  OperationRef, OperationRequest, RegistryPublishRequest, WriteResult,
} from "runnerless/v1";

const program = workflow().releasePlease();

export function configure(): void {
  program.configure();
  configuration.workflow("crate-publishing", ["workflow_run", "runnerless_completion"]);
  configuration.workflow("codex-review", [
    "pull_request", "pull_request_review", "pull_request_review_comment",
    "pull_request_review_thread", "issue_comment", "check_run",
  ]);
}

export function run(): void {
  program.run();
  const details = event.details();
  const name = details.get("event").string();
  if (name == "runnerless_completion") {
    const result = jobs.lookup<WriteResult>(new OperationRef<WriteResult>("op:publish-micro-h2"));
    if (result.ok) {
      const value = result.value;
      checks.report("crates-micro-h2", value.get("status").string() == "succeeded"
        && value.get("value").get("failed").json == "false",
        "crates.io publication: " + value.get("status").string() + " " + value.get("value").get("error").string());
    }
    return;
  }
  if (name == "workflow_run") {
    if (details.get("action").string() != "completed"
      || details.get("workflowPath").string() != ".github/workflows/publish-release.yml"
      || details.get("conclusion").string() != "success") return;
    const version = repo.readText("version.txt", "head").trim();
    const tag = "v" + version;
    if (!packages.releasePublished(tag, version)) return;
    packages.crates(new OperationRequest("publish-micro-h2"),
      new RegistryPublishRequest(tag, version).add("micro-h2", "micro-h2-" + version + ".cargo-upload"));
    return;
  }
  if (details.get("pullNumber").isNull() || details.get("state").string() != "open") return;
  const review = reviews.evaluate();
  checks.report("Codex review", review.state == "success", review.summary);
}
