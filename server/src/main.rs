use std::path::PathBuf;
use anyhow::{Result, bail, anyhow};
use getopts::Options;
use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use std::result::Result as SResult;
use std::sync::Arc;
use chrono::prelude::*;
#[allow(unused_imports)]
use slog::{debug, info, warn, error, Logger, o};
use std::any::Any;
#[allow(unused_imports)]
use keeper_common::*;
use tokio::sync::RwLock;
use hyper::{Request, Body};

use dropshot::{
    ConfigLogging,
    ConfigLoggingLevel,
    ConfigDropshot,
    RequestContext,
    ApiDescription,
    HttpServer,
    HttpError,
    HttpResponseCreated,
    endpoint,
    TypedBody,
};
use hyper::{StatusCode, header::AUTHORIZATION};

mod store;
use store::*;

trait MakeInternalError<T> {
    fn or_500(self) -> SResult<T, HttpError>;
}

impl<T> MakeInternalError<T> for std::io::Result<T> {
    fn or_500(self) -> SResult<T, HttpError> {
        self.map_err(|e| {
            let msg = format!("internal error: {:?}", e);
            HttpError::for_internal_error(msg)
        })
    }
}

impl<T> MakeInternalError<T> for std::result::Result<T, anyhow::Error> {
    fn or_500(self) -> SResult<T, HttpError> {
        self.map_err(|e| {
            let msg = format!("internal error: {:?}", e);
            HttpError::for_internal_error(msg)
        })
    }
}

struct App {
    #[allow(dead_code)]
    log: Logger,
    keys: RwLock<KeyStore>,
    reports: RwLock<ReportStore>,
}

impl App {
    fn from_private(ctx: Arc<dyn Any + Send + Sync + 'static>) -> Arc<App> {
        ctx.downcast::<App>().expect("app downcast")
    }

    fn from_request(rqctx: &Arc<RequestContext>) -> Arc<App> {
        Self::from_private(Arc::clone(&rqctx.server.private))
    }

    async fn require_auth(&self, req: &Request<Body>)
        -> SResult<Auth, HttpError>
    {
        let v = if let Some(h) = req.headers().get(AUTHORIZATION) {
            if let Ok(v) = h.to_str() {
                Some(v.to_string())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(v) = v {
            let t = v.split_whitespace().map(|s| s.trim()).collect::<Vec<_>>();

            if t.len() == 2 && t.iter().all(|s| !s.is_empty()) {
                let keys = self.keys.read().await;

                if t[0].to_lowercase().trim() == "bearer" {
                    match keys.check_key(t[1]) {
                        Ok(Some(auth)) => return Ok(auth),
                        Ok(None) => (),
                        Err(e) => {
                            let msg = format!("internal error: {:?}", e);
                            return Err(HttpError::for_internal_error(msg));
                        }
                    }
                }
            }
        }

        Err(HttpError::for_client_error(None, StatusCode::UNAUTHORIZED,
            "invalid Authorization header".into()))
    }
}

#[derive(Deserialize, JsonSchema)]
struct EnrolBody {
    host: String,
    key: String,
}

#[endpoint {
    method = POST,
    path = "/enrol",
}]
async fn enrol(
    arc: Arc<RequestContext>,
    body: TypedBody<EnrolBody>)
    -> SResult<HttpResponseCreated<()>, HttpError>
{
    let body = body.into_inner();
    let app = App::from_request(&arc);

    if !key_ok(&body.key) {
        return Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "invalid key format".into()));
    }
    if !name_ok(&body.host) {
        return Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "invalid name format".into()));
    }

    let keys = app.keys.write().await;
    if keys.enrol_key(&body.host, &body.key).or_500()? {
        Ok(HttpResponseCreated(()))
    } else {
        Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "invalid enrolment request".into()))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReportId {
    host: String,
    job: String,
    pid: u64,
    time: DateTime<Utc>,
    uuid: String,
}

#[derive(Deserialize, JsonSchema)]
struct ReportStartBody {
    id: ReportId,
    start_time: DateTime<Utc>,
    script: String,
}

#[derive(Serialize, JsonSchema)]
struct ReportResult {
    existed_already: bool,
}

#[endpoint {
    method = POST,
    path = "/report/start",
}]
async fn report_start(
    arc: Arc<RequestContext>,
    body: TypedBody<ReportStartBody>)
    -> SResult<HttpResponseCreated<ReportResult>, HttpError>
{
    let body = body.into_inner();
    let app = App::from_request(&arc);

    let req = arc.request.lock().await;
    let auth = app.require_auth(&req).await?;
    if body.id.host != auth.host {
        return Err(HttpError::for_client_error(None, StatusCode::UNAUTHORIZED,
            "uh uh uh".into()));
    }

    if !name_ok(&body.id.job) {
        return Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "job name too short".into()));
    }
    /*
     * XXX check that job time is in the last fornight, or whatever
     */

    let reports = app.reports.write().await;
    match reports.load(&body.id.host, &body.id.job, &body.id.time) {
        Ok(Some(f)) => {
            /*
             * A report for this time exists already.  Check to make sure that
             * the report UUID is the same as what the client sent; if it is, we
             * can return success, but if not we should return a conflict.
             */
            if body.id.uuid != f.report_uuid {
                Err(HttpError::for_client_error(None,
                    StatusCode::CONFLICT,
                    "this time already submitted, with different UUID".into()))
            } else if f.sealed {
                Err(HttpError::for_client_error(None,
                    StatusCode::CONFLICT,
                    "this job is already complete".into()))
            } else {
                Ok(HttpResponseCreated(ReportResult {
                    existed_already: true,
                }))
            }
        }
        Ok(None) => {
            /*
             * A report for this time does not exist, so we can accept what the
             * client has sent!
             */
            let pf = PostFile {
                sealed: false,
                report_uuid: body.id.uuid,
                report_time: Utc::now(),
                report_pid: body.id.pid,
                time_start: body.start_time,
                time_end: None,
                duration: None,
                status: None,
                output: Vec::new(),
                script: body.script,
            };
            if let Err(e) = reports.store(&body.id.host, &body.id.job,
                &body.id.time, &pf)
            {
                Err(HttpError::for_internal_error(
                    format!("store file? {:?}", e)))
            } else {
                Ok(HttpResponseCreated(ReportResult {
                    existed_already: false,
                }))
            }
        }
        Err(e) => {
            error!(arc.log, "load file error: {:?}", e);
            Err(HttpError::for_internal_error("data store error".into()))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReportOutputBody {
    id: ReportId,
    record: OutputRecord,
}

#[endpoint {
    method = POST,
    path = "/report/output",
}]
async fn report_output(
    arc: Arc<RequestContext>,
    body: TypedBody<ReportOutputBody>)
    -> SResult<HttpResponseCreated<ReportResult>, HttpError>
{
    let body = body.into_inner();
    let app = App::from_request(&arc);

    let req = arc.request.lock().await;
    let auth = app.require_auth(&req).await?;
    if body.id.host != auth.host {
        return Err(HttpError::for_client_error(None, StatusCode::UNAUTHORIZED,
            "uh uh uh".into()));
    }

    if !name_ok(&body.id.job) {
        return Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "job name too short".into()));
    }

    /*
     * XXX check that job time is in the last fornight, or whatever
     */

    let reports = app.reports.write().await;
    match reports.load(&body.id.host, &body.id.job, &body.id.time) {
        Ok(Some(mut f)) => {
            /*
             * A report for this time exists already.  Check to make sure that
             * the report UUID is the same as what the client sent; if it is, we
             * can return success, but if not we should return a conflict.
             */
            if body.id.uuid != f.report_uuid {
                Err(HttpError::for_client_error(None,
                    StatusCode::CONFLICT,
                    "this time already submitted, with different UUID".into()))
            } else if f.sealed {
                Err(HttpError::for_client_error(None,
                    StatusCode::CONFLICT,
                    "this job is already complete".into()))
            } else {
                /*
                 * This job exists and the UUID matches the one recorded when
                 * the record was created.  Check to make sure the output
                 * record does not already appear in the file.
                 */
                if f.output.contains(&body.record) {
                    Ok(HttpResponseCreated(ReportResult {
                        existed_already: true,
                    }))
                } else {
                    f.output.push(body.record);

                    if let Err(e) = reports.store(&body.id.host,
                        &body.id.job, &body.id.time, &f)
                    {
                        Err(HttpError::for_internal_error(
                            format!("store file? {:?}", e)))
                    } else {
                        Ok(HttpResponseCreated(ReportResult {
                            existed_already: false,
                        }))
                    }
                }
            }
        }
        Ok(None) => {
            /*
             * If the job file does not exist already, we cannot append an
             * output record to it.
             */
            Err(HttpError::for_client_error(None,
                StatusCode::BAD_REQUEST,
                "this job does not exist".into()))
        }
        Err(e) => {
            error!(arc.log, "load file error: {:?}", e);
            Err(HttpError::for_internal_error("data store error".into()))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReportFinishBody {
    id: ReportId,

    end_time: DateTime<Utc>,
    duration_millis: i32,
    exit_status: i32,
}

#[endpoint {
    method = POST,
    path = "/report/finish",
}]
async fn report_finish(
    arc: Arc<RequestContext>,
    body: TypedBody<ReportFinishBody>)
    -> SResult<HttpResponseCreated<ReportResult>, HttpError>
{
    let body = body.into_inner();
    let app = App::from_request(&arc);

    let req = arc.request.lock().await;
    let auth = app.require_auth(&req).await?;
    if body.id.host != auth.host {
        return Err(HttpError::for_client_error(None, StatusCode::UNAUTHORIZED,
            "uh uh uh".into()));
    }

    if !name_ok(&body.id.job) {
        return Err(HttpError::for_client_error(None, StatusCode::BAD_REQUEST,
            "job name too short".into()));
    }

    /*
     * XXX check that job time is in the last fornight, or whatever
     */
    let reports = app.reports.write().await;
    match reports.load(&body.id.host, &body.id.job, &body.id.time) {
        Ok(Some(mut f)) => {
            /*
             * A report for this time exists already.  Check to make sure that
             * the report UUID is the same as what the client sent; if it is, we
             * can return success, but if not we should return a conflict.
             */
            if body.id.uuid != f.report_uuid {
                Err(HttpError::for_client_error(None,
                    StatusCode::CONFLICT,
                    "this time already submitted, with different UUID".into()))
            } else if f.sealed {
                Ok(HttpResponseCreated(ReportResult {
                    existed_already: true,
                }))
            } else {
                f.duration = Some(body.duration_millis as u64);
                f.time_end = Some(body.end_time);
                f.status = Some(body.exit_status);
                f.sealed = true;

                if let Err(e) = reports.store(&body.id.host, &body.id.job,
                    &body.id.time, &f)
                {
                    Err(HttpError::for_internal_error(
                        format!("store file? {:?}", e)))
                } else {
                    Ok(HttpResponseCreated(ReportResult {
                        existed_already: false,
                    }))
                }
            }
        }
        Ok(None) => {
            /*
             * If the job file does not exist already, we cannot append an
             * output record to it.
             */
            Err(HttpError::for_client_error(None,
                StatusCode::BAD_REQUEST,
                "this job does not exist".into()))
        }
        Err(e) => {
            error!(arc.log, "load file error: {:?}", e);
            Err(HttpError::for_internal_error("data store error".into()))
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct GlobalJobsResult {
    summary: Vec<ReportSummary>,
}

#[endpoint {
    method = GET,
    path = "/global/jobs",
}]
async fn global_jobs(
    arc: Arc<RequestContext>,
) -> SResult<HttpResponseCreated<GlobalJobsResult>, HttpError>
{
    let app = App::from_request(&arc);

    let req = arc.request.lock().await;
    let auth = app.require_auth(&req).await?;
    if !auth.global_view {
        return Err(HttpError::for_client_error(None, StatusCode::UNAUTHORIZED,
            "uh uh uh".into()));
    }

    let reports = app.reports.read().await;
    let summary = reports.summary(1).or_500()?;

    Ok(HttpResponseCreated(GlobalJobsResult {
        summary,
    }))
}


#[tokio::main]
async fn main() -> Result<()> {
    let mut opts = Options::new();

    opts.optopt("b", "", "bind address:port", "BIND_ADDRESS");
    opts.optopt("d", "", "data directory", "DIRECTORY");
    opts.optopt("S", "", "dump OpenAPI schema", "FILE");

    let p = match opts.parse(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: usage: {}", e);
            eprintln!("       {}", opts.usage("usage"));
            std::process::exit(1);
        }
    };

    let mut api = ApiDescription::new();
    api.register(enrol).unwrap();
    api.register(report_start).unwrap();
    api.register(report_output).unwrap();
    api.register(report_finish).unwrap();
    api.register(global_jobs).unwrap();

    if let Some(s) = p.opt_str("S") {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&s)?;
        api.openapi("Keeper API", "1.0")
            .description("report execution of cron jobs through a \
                mechanism other than mail")
            .contact_name("Joshua M. Clulow")
            .contact_url("https://github.com/jclulow/keeper")
            .write(&mut f)?;
        return Ok(());
    }

    let bind = p.opt_str("b").unwrap_or_else(|| String::from("0.0.0.0:9978"));
    let dir = if let Some(d) = p.opt_str("d") {
        PathBuf::from(d)
    } else {
        bail!("ERROR: must specify data directory (-d)");
    };
    if !dir.is_dir() {
        bail!("ERROR: {} should be a directory", dir.display());
    }

    let cfglog = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    };
    let log = cfglog.to_logger("keeper")?;

    let keylog = log.new(o!("component" => "keystore"));
    let keys = RwLock::new(KeyStore::new(keylog, dir.clone())?);

    let reportlog = log.new(o!("component" => "reportstore"));
    let reports = RwLock::new(ReportStore::new(reportlog, dir.clone())?);

    let app = Arc::new(App {
        log: log.clone(),
        keys,
        reports,
    });

    let cfgds = ConfigDropshot {
        bind_address: bind.parse()?,
        ..Default::default()
    };

    let mut server = HttpServer::new(&cfgds, api, app, &log)?;
    let task = server.run();
    server.wait_for_shutdown(task).await
        .map_err(|e| anyhow!("server task failure: {:?}", e))?;
    bail!("early exit is unexpected");
}
