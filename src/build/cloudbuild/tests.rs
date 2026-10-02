//! Cloud Build orchestration tests against an in-memory stub (`CloudBuild::from_stub`).

use super::*;
use crate::gcp::registry::ResolveError;
use google_cloud_build_v1::model::{
    BuiltImage as ApiBuiltImage, CancelBuildRequest, CreateBuildRequest, GetBuildRequest,
    ListBuildsRequest, ListBuildsResponse, Results,
};
use google_cloud_gax::error::rpc::{Code, Status};
use google_cloud_gax::options::RequestOptions;
use google_cloud_gax::response::Response;
use google_cloud_longrunning::model::Operation;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type GaxError = google_cloud_gax::error::Error;
type R<T> = google_cloud_gax::Result<Response<T>>;

/// Hash of the fixture produced by [`fixture`] (deterministic).
const HASH: &str = "5ce484b4bc34d08c7375ef268d8301111099d83296b98acae6b1c75fab70500c";
const TAGGED: &str = "europe-west1-docker.pkg.dev/p/apps/hello:src-5ce484b4bc34d08c";
const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Debug, Default)]
struct BuildState {
    creates: Vec<CreateBuildRequest>,
    cancels: Vec<CancelBuildRequest>,
    builds: Vec<Build>,
    create_errors: VecDeque<GaxError>,
    apply_then_error: bool,
    /// Statuses returned by successive get_build calls.
    statuses: VecDeque<build::Status>,
    final_detail: String,
}

#[derive(Debug, Clone, Default)]
struct FakeBuild(Arc<Mutex<BuildState>>);

fn running_build(id: &str, status: build::Status) -> Build {
    Build::new()
        .set_id(id)
        .set_project_id("p")
        .set_status(status)
        .set_tags(naming::build_tags("hello", "dev", HASH))
        .set_images([TAGGED])
        .set_log_url(format!(
            "https://console.cloud.google.com/cloud-build/builds/{id}"
        ))
}

impl google_cloud_build_v1::stub::CloudBuild for FakeBuild {
    async fn create_build(&self, req: CreateBuildRequest, _o: RequestOptions) -> R<Operation> {
        let mut st = self.0.lock().unwrap();
        st.creates.push(req.clone());
        let err = match st.create_errors.pop_front() {
            Some(e) if !st.apply_then_error => return Err(e),
            other => other,
        };
        let id = format!("b-{}", st.creates.len());
        let b = req
            .build
            .unwrap()
            .set_id(&id)
            .set_status(build::Status::Queued);
        st.builds.push(b.clone());
        if let Some(e) = err {
            return Err(e);
        }
        let md = BuildOperationMetadata::new().set_build(b);
        Ok(Response::from(
            Operation::new()
                .set_name("operations/build/1")
                .set_metadata(google_cloud_wkt::Any::from_msg(&md).unwrap()),
        ))
    }

    async fn get_build(&self, req: GetBuildRequest, _o: RequestOptions) -> R<Build> {
        let mut st = self.0.lock().unwrap();
        assert!(
            req.name
                .starts_with("projects/p/locations/europe-west1/builds/"),
            "regional build name"
        );
        assert!(
            req.project_id.is_empty() && req.id.is_empty(),
            "regional binding only"
        );
        let id = req.name.rsplit('/').next().unwrap().to_string();
        let status = st.statuses.pop_front().unwrap_or(build::Status::Success);
        let mut b = running_build(&id, status.clone());
        if status == build::Status::Success {
            b = b.set_results(
                Results::new()
                    .set_images([ApiBuiltImage::new().set_name(TAGGED).set_digest(DIGEST)]),
            );
        } else if matches!(status, build::Status::Failure | build::Status::Timeout) {
            b = b
                .set_status_detail(st.final_detail.clone())
                .set_steps([BuildStep::new()
                    .set_name(DOCKER_BUILDER)
                    .set_args(["build", "-t", TAGGED])
                    .set_status(build::Status::Failure)]);
        }
        Ok(Response::from(b))
    }

    async fn list_builds(
        &self,
        req: ListBuildsRequest,
        _o: RequestOptions,
    ) -> R<ListBuildsResponse> {
        assert_eq!(req.parent, "projects/p/locations/europe-west1");
        assert!(req.project_id.is_empty());
        assert_eq!(req.filter, "tags=\"runway-src-5ce484b4bc34d08c\"");
        let st = self.0.lock().unwrap();
        Ok(Response::from(
            ListBuildsResponse::new().set_builds(st.builds.clone()),
        ))
    }

    async fn cancel_build(&self, req: CancelBuildRequest, _o: RequestOptions) -> R<Build> {
        let mut st = self.0.lock().unwrap();
        st.cancels.push(req);
        Ok(Response::from(Build::new()))
    }
}

#[derive(Default)]
struct FakeUploader(Mutex<Vec<(String, String, usize)>>);

#[async_trait]
impl SourceUploader for FakeUploader {
    async fn upload(&self, bucket: &str, object: &str, archive: &CompressedArchive) -> Result<i64> {
        // The uploaded bytes must be a valid gzip stream of the scanned tar.
        let bytes = archive.read_all()?;
        assert_eq!(bytes.len() as u64, archive.len);
        let mut tar = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(bytes.as_slice()),
            &mut tar,
        )
        .unwrap();
        assert!(!tar.is_empty());
        self.0
            .lock()
            .unwrap()
            .push((bucket.into(), object.into(), bytes.len()));
        Ok(42)
    }
}

struct FakeResolver(std::result::Result<Option<String>, ResolveError>);

#[async_trait]
impl DigestResolver for FakeResolver {
    async fn resolve(
        &self,
        _image: &ImageRef,
    ) -> std::result::Result<Option<String>, ResolveError> {
        self.0.clone()
    }
}

fn config() -> BuildConfig {
    BuildConfig {
        context_dir: ".".into(),
        strategy: BuildStrategy::Dockerfile {
            path: "docker/Dockerfile".into(),
        },
        artifact_location: "europe-west1".into(),
        artifact_repository: "apps".into(),
        source_bucket: "my-sources".into(),
        build_service_account: "builds@p.iam.gserviceaccount.com".into(),
        excluded: vec![],
        create_resources: false,
        rebuild_always: false,
    }
}

/// A tiny build context with a fixed content hash ([`HASH`]).
fn fixture() -> (tempfile::TempDir, SourceManifest) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("Dockerfile"),
        "FROM scratch\nCOPY main.py /\n",
    )
    .unwrap();
    std::fs::write(d.path().join("main.py"), "print('hello')\n").unwrap();
    let m = package::scan(
        d.path(),
        &BuildStrategy::Dockerfile {
            path: "Dockerfile".into(),
        },
        &[],
    )
    .unwrap();
    assert_eq!(m.sha256, HASH, "fixture hash changed");
    (d, m)
}

struct Env {
    fake: FakeBuild,
    client: CloudBuild,
    uploader: FakeUploader,
    resolver: FakeResolver,
    progress: Progress,
}

impl Env {
    fn new(resolved: std::result::Result<Option<String>, ResolveError>) -> Self {
        let fake = FakeBuild::default();
        Self {
            client: CloudBuild::from_stub(fake.clone()),
            fake,
            uploader: FakeUploader::default(),
            resolver: FakeResolver(resolved),
            progress: Progress::silent(),
        }
    }

    async fn run(&self, force: bool) -> Result<BuiltImage> {
        self.run_with(force, false).await
    }

    async fn run_with(&self, force: bool, image_checked: bool) -> Result<BuiltImage> {
        let cfg = config();
        let (_dir, source) = fixture();
        let inp = BuildInputs {
            project: "p",
            region: "europe-west1",
            app: "hello",
            stage: "dev",
            config: &cfg,
            source: &source,
            image_checked,
            timeout: Duration::from_secs(600),
            force,
        };
        Builder {
            cloudbuild: &self.client,
            uploader: &self.uploader,
            resolver: &self.resolver,
            logging: None,
            progress: &self.progress,
            poll: PollConfig::fast(),
        }
        .build(&inp)
        .await
    }
}

#[tokio::test]
async fn uploads_submits_and_returns_pushed_digest() {
    let env = Env::new(Ok(None));
    env.fake.0.lock().unwrap().statuses = VecDeque::from([
        build::Status::Queued,
        build::Status::Working,
        build::Status::Success,
    ]);
    let out = env.run(false).await.unwrap();
    assert_eq!(out.digest, DIGEST);
    assert_eq!(
        out.pinned,
        format!("europe-west1-docker.pkg.dev/p/apps/hello@{DIGEST}")
    );
    assert_eq!(out.tagged, TAGGED);
    assert!(!out.reused);
    assert_eq!(out.build_id.as_deref(), Some("b-1"));

    let uploads = env.uploader.0.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert!(uploads[0].2 > 0);
    assert_eq!(uploads[0].0, "my-sources");
    assert_eq!(uploads[0].1, format!("runway/hello/source-{HASH}.tar.gz"));

    let st = env.fake.0.lock().unwrap();
    assert_eq!(st.creates.len(), 1);
    let req = &st.creates[0];
    assert_eq!(req.parent, "projects/p/locations/europe-west1");
    assert!(req.project_id.is_empty(), "regional binding only");
    let b = req.build.as_ref().unwrap();
    match b.source.as_ref().unwrap().source.as_ref().unwrap() {
        google_cloud_build_v1::model::source::Source::StorageSource(s) => {
            assert_eq!(s.bucket, "my-sources");
            assert_eq!(s.generation, 42, "pins the uploaded object generation");
        }
        other => panic!("unexpected source {other:?}"),
    }
    assert_eq!(b.steps[0].name, DOCKER_BUILDER);
    assert_eq!(
        b.steps[0].args,
        ["build", "-t", TAGGED, "-f", "docker/Dockerfile", "."]
    );
    assert_eq!(b.images, [TAGGED]);
    assert_eq!(
        b.service_account,
        "projects/p/serviceAccounts/builds@p.iam.gserviceaccount.com"
    );
    assert_eq!(
        b.options.as_ref().unwrap().logging,
        LoggingMode::CloudLoggingOnly
    );
    assert!(b.tags.contains(&"runway-src-5ce484b4bc34d08c".to_string()));
    assert_eq!(b.timeout.as_ref().unwrap().seconds(), 600);
}

#[tokio::test]
async fn reuses_existing_image_without_building() {
    let env = Env::new(Ok(Some(DIGEST.into())));
    let out = env.run(false).await.unwrap();
    assert!(out.reused);
    assert_eq!(out.digest, DIGEST);
    assert!(env.uploader.0.lock().unwrap().is_empty());
    assert!(env.fake.0.lock().unwrap().creates.is_empty());
}

#[tokio::test]
async fn force_rebuilds_even_if_image_exists() {
    let env = Env::new(Ok(Some(DIGEST.into())));
    let out = env.run(true).await.unwrap();
    assert!(!out.reused);
    assert_eq!(env.fake.0.lock().unwrap().creates.len(), 1);
}

#[tokio::test]
async fn attaches_to_in_flight_build_of_same_source() {
    let env = Env::new(Ok(None));
    {
        let mut st = env.fake.0.lock().unwrap();
        st.builds
            .push(running_build("existing", build::Status::Working));
        st.statuses = VecDeque::from([build::Status::Working, build::Status::Success]);
    }
    let out = env.run(false).await.unwrap();
    assert_eq!(out.build_id.as_deref(), Some("existing"));
    assert!(env.uploader.0.lock().unwrap().is_empty());
    assert!(env.fake.0.lock().unwrap().creates.is_empty());
}

#[tokio::test]
async fn never_attaches_to_another_apps_build_of_identical_source() {
    let env = Env::new(Ok(None));
    {
        let mut st = env.fake.0.lock().unwrap();
        // Same source fingerprint (same build tag), different image path.
        st.builds.push(
            running_build("other-app", build::Status::Working)
                .set_images(["europe-west1-docker.pkg.dev/p/apps/other:src-1234"]),
        );
        st.statuses = VecDeque::from([build::Status::Success]);
    }
    let out = env.run(false).await.unwrap();
    assert_ne!(out.build_id.as_deref(), Some("other-app"));
    assert_eq!(
        env.fake.0.lock().unwrap().creates.len(),
        1,
        "built its own image"
    );
    assert!(out.pinned.starts_with(TAGGED.split(':').next().unwrap()));
}

#[tokio::test]
async fn ambiguous_submission_checks_for_existing_build() {
    let env = Env::new(Ok(None));
    {
        let mut st = env.fake.0.lock().unwrap();
        st.apply_then_error = true;
        st.create_errors.push_back(GaxError::service(
            Status::default()
                .set_code(Code::Unavailable)
                .set_message("connection reset"),
        ));
    }
    let out = env.run(false).await.unwrap();
    assert_eq!(out.build_id.as_deref(), Some("b-1"));
    assert_eq!(
        env.fake.0.lock().unwrap().creates.len(),
        1,
        "not resubmitted"
    );
}

#[tokio::test]
async fn failed_build_reports_actionable_error() {
    let env = Env::new(Ok(None));
    {
        let mut st = env.fake.0.lock().unwrap();
        st.statuses = VecDeque::from([build::Status::Working, build::Status::Failure]);
        st.final_detail = "Build step failure: build step 0 \"gcr.io/cloud-builders/docker\" failed: step exited with non-zero status: 1".into();
    }
    let err = env.run(false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Build);
    assert!(err.message.contains("FAILURE"), "{}", err.message);
    assert!(err.message.contains("non-zero status"), "{}", err.message);
    assert!(
        err.message
            .contains("failed step: gcr.io/cloud-builders/docker"),
        "{}",
        err.message
    );
    assert!(
        err.hints
            .iter()
            .any(|h| h.contains("cloud-build/builds/b-1"))
    );
}

#[tokio::test]
async fn build_timeout_status_maps_to_timeout() {
    let env = Env::new(Ok(None));
    env.fake.0.lock().unwrap().statuses = VecDeque::from([build::Status::Timeout]);
    let err = env.run(false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
}

#[tokio::test]
async fn permission_error_on_submit_is_not_retried() {
    let env = Env::new(Ok(None));
    env.fake
        .0
        .lock()
        .unwrap()
        .create_errors
        .push_back(GaxError::service(
            Status::default()
                .set_code(Code::PermissionDenied)
                .set_message("caller does not have permission to act as service account"),
        ));
    let err = env.run(false).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Prerequisite);
    assert!(
        err.hints
            .iter()
            .any(|h| h.contains("iam.serviceAccounts.actAs"))
    );
    assert_eq!(env.fake.0.lock().unwrap().creates.len(), 1);
}

#[tokio::test]
async fn skips_registry_recheck_when_caller_already_checked() {
    // The resolver would report an existing image, but the caller says it
    // already checked (and found none), so no second registry request is made.
    let env = Env::new(Ok(Some(DIGEST.into())));
    let out = env.run_with(false, true).await.unwrap();
    assert!(!out.reused);
    assert_eq!(env.fake.0.lock().unwrap().creates.len(), 1);
}

#[tokio::test]
async fn nothing_is_uploaded_when_attaching_to_in_flight_build() {
    let env = Env::new(Ok(None));
    {
        let mut st = env.fake.0.lock().unwrap();
        st.builds
            .push(running_build("running", build::Status::Working));
        st.statuses = VecDeque::from([build::Status::Success]);
    }
    env.run(false).await.unwrap();
    assert!(env.uploader.0.lock().unwrap().is_empty());
}

#[test]
fn buildpacks_build_step_uses_pack_and_pushes_through_images() {
    let step = build_step(
        &BuildStrategy::Buildpacks {
            builder: "gcr.io/buildpacks/builder:latest".into(),
        },
        TAGGED,
    );
    assert_eq!(step.name, PACK_BUILDER);
    assert_eq!(step.entrypoint, "pack");
    assert_eq!(
        step.args,
        [
            "build",
            TAGGED,
            "--builder",
            "gcr.io/buildpacks/builder:latest",
            "--network",
            "cloudbuild",
            "--path",
            "."
        ]
    );
    assert!(
        !step.args.iter().any(|a| a == "--publish"),
        "Cloud Build pushes `images` and reports digests"
    );
}
