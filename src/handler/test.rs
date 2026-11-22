// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(clippy::unwrap_used)]

use crate::{asset_handler, scan_error_response};
use axum::{
    body::to_bytes,
    extract::{Path, State},
    response::IntoResponse,
};
use http::{HeaderMap, StatusCode, header};
use pretty_assertions::assert_eq;
use std::{borrow::Cow, collections::HashMap};

#[tokio::test]
async fn handle_assets_ok() {
    let path = "a.json".to_owned();
    let files = HashMap::from([("a.json".to_owned(), Cow::from("test".as_bytes()))]);
    let response = asset_handler(Path(path), HeaderMap::new(), State((files, String::new())))
        .await
        .into_response();

    assert_eq!(StatusCode::OK, response.status());
    assert_eq!(
        "application/json",
        response.headers().get(header::CONTENT_TYPE).unwrap()
    );
    assert_eq!(
        "test",
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    );
}

#[tokio::test]
async fn handle_assets_404() {
    let path = "x".to_owned();
    let files = HashMap::from([("a".to_owned(), Cow::from("test".as_bytes()))]);
    let response = asset_handler(Path(path), HeaderMap::new(), State((files, String::new())))
        .await
        .into_response();

    assert_eq!(StatusCode::NOT_FOUND, response.status());
    assert_eq!(
        "404",
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    );
}

#[test]
fn scan_error_status_mapping() {
    assert_eq!(
        StatusCode::BAD_REQUEST,
        scan_error_response(&scan::ScanError::InvalidHost("x".into())).status()
    );
    assert_eq!(
        StatusCode::CONFLICT,
        scan_error_response(&scan::ScanError::Busy).status()
    );
    assert_eq!(
        StatusCode::INTERNAL_SERVER_ERROR,
        scan_error_response(&scan::ScanError::Timeout).status()
    );
}

// ---- ptz_handler ----

mod ptz {
    use crate::{MonitorIdQuery, PtzRouteState, PtzService, ptz_handler};
    use async_trait::async_trait;
    use axum::{
        Json,
        body::to_bytes,
        extract::{Query, State},
    };
    use common::{
        MonitorId,
        monitor::{
            IMonitorManager, MonitorConfig, MonitorConfigs, MonitorDeleteError, MonitorInfo,
            MonitorRestartError, MonitorSetAndRestartError, MonitorSetError,
        },
    };
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    struct StubManager {
        config: Mutex<Option<MonitorConfig>>,
    }

    #[async_trait]
    impl IMonitorManager for StubManager {
        async fn monitor_restart(
            &self,
            _monitor_id: MonitorId,
        ) -> Option<Result<(), MonitorRestartError>> {
            unimplemented!()
        }
        async fn monitor_set(
            &self,
            _config: MonitorConfig,
        ) -> Option<Result<bool, MonitorSetError>> {
            unimplemented!()
        }
        async fn monitor_set_and_restart(
            &self,
            _config: MonitorConfig,
        ) -> Option<Result<bool, MonitorSetAndRestartError>> {
            unimplemented!()
        }
        async fn monitor_delete(&self, _id: MonitorId) -> Option<Result<(), MonitorDeleteError>> {
            unimplemented!()
        }
        async fn monitors_info(&self) -> Option<HashMap<MonitorId, MonitorInfo>> {
            unimplemented!()
        }
        async fn monitor_config(&self, _monitor_id: MonitorId) -> Option<Option<MonitorConfig>> {
            Some(self.config.lock().unwrap().clone())
        }
        async fn monitor_configs(&self) -> Option<MonitorConfigs> {
            unimplemented!()
        }
    }

    struct StubPtz {
        err: bool,
        presets: Vec<ptz::Preset>,
    }

    #[async_trait]
    impl ptz::PtzClient for StubPtz {
        async fn move_cam(
            &self,
            _pan: Option<f64>,
            _tilt: Option<f64>,
            _zoom: Option<f64>,
        ) -> Result<(), ptz::Error> {
            if self.err {
                return Err(ptz::Error::NoPtzService);
            }
            Ok(())
        }
        async fn stop(&self) -> Result<(), ptz::Error> {
            Ok(())
        }
        async fn home(&self) -> Result<(), ptz::Error> {
            Ok(())
        }
        async fn get_presets(&self) -> Result<Vec<ptz::Preset>, ptz::Error> {
            Ok(self.presets.clone())
        }
        async fn set_preset(&self, _name: &str) -> Result<String, ptz::Error> {
            Ok("1".into())
        }
        async fn goto_preset(&self, _token: &str) -> Result<(), ptz::Error> {
            Ok(())
        }
        async fn remove_preset(&self, _token: &str) -> Result<(), ptz::Error> {
            Ok(())
        }
    }

    fn cmd(command: &str) -> ptz::PtzCommand {
        ptz::PtzCommand {
            command: command.to_owned(),
            pan: None,
            tilt: None,
            zoom: None,
            preset_token: None,
            preset_name: None,
        }
    }

    fn monitor_config(enable_ptz: bool) -> MonitorConfig {
        use common::monitor::{
            Config, Protocol, PtzConfig, SelectedSource, SourceConfig, SourceRtspConfig,
        };
        MonitorConfig::new(
            Config {
                id: "m1".to_owned().try_into().unwrap(),
                name: "m".to_owned().try_into().unwrap(),
                enable: true,
                source: SelectedSource::Rtsp,
                always_record: false,
                video_length: 0.0,
                ptz: PtzConfig::new(enable_ptz, String::new()),
            },
            SourceConfig::Rtsp(SourceRtspConfig {
                protocol: Protocol::Tcp,
                main_stream: "rtsp://u%40x:p%40ss@cam/main".parse().unwrap(),
                sub_stream: None,
            }),
            serde_json::json!({}),
        )
    }

    type Captured = Arc<Mutex<(String, (String, String))>>;

    fn harness(
        config: Option<MonitorConfig>,
        stub: ptz::ArcPtzClient,
    ) -> (State<Arc<PtzRouteState>>, Query<MonitorIdQuery>, Captured) {
        let captured: Captured =
            Arc::new(Mutex::new((String::new(), (String::new(), String::new()))));
        let cap = captured.clone();
        (
            State(Arc::new(PtzRouteState {
                monitors: Arc::new(StubManager {
                    config: Mutex::new(config),
                }),
                service: PtzService::new(Arc::new(move |endpoint, creds| {
                    *cap.lock().unwrap() = (endpoint, creds);
                    let client: ptz::ArcPtzClient = stub.clone();
                    client
                })),
            })),
            Query(MonitorIdQuery {
                id: "m1".to_owned().try_into().unwrap(),
            }),
            captured,
        )
    }

    fn stub_client(err: bool) -> ptz::ArcPtzClient {
        Arc::new(StubPtz {
            err,
            presets: vec![],
        })
    }

    #[tokio::test]
    async fn ptz_disabled_is_409() {
        let (state, query, _) = harness(Some(monitor_config(false)), stub_client(false));
        let resp = ptz_handler(state, query, Json(cmd("stop"))).await;
        assert_eq!(StatusCode::CONFLICT, resp.status());
    }

    #[tokio::test]
    async fn ptz_unknown_command_is_400() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(false));
        let resp = ptz_handler(state, query, Json(cmd("bogus"))).await;
        assert_eq!(StatusCode::BAD_REQUEST, resp.status());
    }

    #[tokio::test]
    async fn ptz_move_without_velocity_is_400() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(false));
        let resp = ptz_handler(state, query, Json(cmd("move"))).await;
        assert_eq!(StatusCode::BAD_REQUEST, resp.status());
    }

    #[tokio::test]
    async fn ptz_velocity_out_of_range_is_400() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(false));
        let mut c = cmd("move");
        c.pan = Some(1.5);
        let resp = ptz_handler(state, query, Json(c)).await;
        assert_eq!(StatusCode::BAD_REQUEST, resp.status());
    }

    #[tokio::test]
    async fn ptz_missing_monitor_is_404() {
        let (state, query, _) = harness(None, stub_client(false));
        let resp = ptz_handler(state, query, Json(cmd("stop"))).await;
        assert_eq!(StatusCode::NOT_FOUND, resp.status());
    }

    #[tokio::test]
    async fn ptz_camera_fault_is_502() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(true));
        let mut c = cmd("move");
        c.pan = Some(0.5);
        let resp = ptz_handler(state, query, Json(c)).await;
        assert_eq!(StatusCode::BAD_GATEWAY, resp.status());
    }

    #[tokio::test]
    async fn ptz_move_ok_decodes_creds() {
        let (state, query, captured) = harness(Some(monitor_config(true)), stub_client(false));
        let mut c = cmd("move");
        c.tilt = Some(-0.5);
        let resp = ptz_handler(state, query, Json(c)).await;
        assert_eq!(StatusCode::OK, resp.status());

        let (endpoint, (user, pass)) = captured.lock().unwrap().clone();
        assert_eq!("http://cam:80/onvif/device_service", endpoint);
        assert_eq!("u@x", user);
        assert_eq!("p@ss", pass);
    }

    #[tokio::test]
    async fn ptz_get_presets_returns_list() {
        let stub = Arc::new(StubPtz {
            err: false,
            presets: vec![ptz::Preset {
                token: "1".into(),
                name: "Gate".into(),
            }],
        });
        let (state, query, _) = harness(Some(monitor_config(true)), stub);
        let resp = ptz_handler(state, query, Json(cmd("getPresets"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["presets"][0]["token"], "1");
        assert_eq!(json["presets"][0]["name"], "Gate");
    }

    // Slow stub client: `move_cam` sleeps before recording — without
    // per-monitor serialization a following `stop` would land first.
    struct OrderedStub {
        ops: Arc<Mutex<Vec<String>>>,
        delay: std::time::Duration,
    }

    #[async_trait]
    impl ptz::PtzClient for OrderedStub {
        async fn move_cam(
            &self,
            _p: Option<f64>,
            _t: Option<f64>,
            _z: Option<f64>,
        ) -> Result<(), ptz::Error> {
            tokio::time::sleep(self.delay).await;
            self.ops.lock().unwrap().push("move".to_owned());
            Ok(())
        }
        async fn stop(&self) -> Result<(), ptz::Error> {
            self.ops.lock().unwrap().push("stop".to_owned());
            Ok(())
        }
        async fn home(&self) -> Result<(), ptz::Error> {
            Ok(())
        }
        async fn get_presets(&self) -> Result<Vec<ptz::Preset>, ptz::Error> {
            Ok(vec![])
        }
        async fn set_preset(&self, _n: &str) -> Result<String, ptz::Error> {
            Ok("1".into())
        }
        async fn goto_preset(&self, _t: &str) -> Result<(), ptz::Error> {
            Ok(())
        }
        async fn remove_preset(&self, _t: &str) -> Result<(), ptz::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn commands_serialized_per_monitor() {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let stub = OrderedStub {
            ops: ops.clone(),
            delay: std::time::Duration::from_millis(100),
        };
        let (state, query, _) = harness(Some(monitor_config(true)), Arc::new(stub));

        let s1 = state.clone();
        let t1 = tokio::spawn(async move {
            let mut c = cmd("move");
            c.pan = Some(0.5);
            ptz_handler(s1, query, Json(c)).await
        });
        // Let t1 grab the monitor lock first.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let q2 = Query(MonitorIdQuery {
            id: "m1".to_owned().try_into().unwrap(),
        });
        let t2 = tokio::spawn(async move { ptz_handler(state, q2, Json(cmd("stop"))).await });

        t1.await.unwrap();
        t2.await.unwrap();
        assert_eq!(
            vec!["move".to_owned(), "stop".to_owned()],
            *ops.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn entry_rebuilt_on_config_change() {
        let calls = Arc::new(Mutex::new(0_usize));
        let c = calls.clone();
        let service = PtzService::new(Arc::new(move |_e, _c| {
            *c.lock().unwrap() += 1;
            stub_client(false)
        }));
        let manager = Arc::new(StubManager {
            config: Mutex::new(Some(monitor_config(true))),
        });
        let state = Arc::new(PtzRouteState {
            monitors: manager.clone(),
            service,
        });
        let query = || {
            Query(MonitorIdQuery {
                id: "m1".to_owned().try_into().unwrap(),
            })
        };

        let st = State(state.clone());
        let resp = ptz_handler(st.clone(), query(), Json(cmd("stop"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        let resp = ptz_handler(st, query(), Json(cmd("stop"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        // Same config → same entry, factory ran once.
        assert_eq!(1, *calls.lock().unwrap());

        // Config edit (onvifAddr) → endpoint key changes → entry rebuilt.
        let edited = monitor_config(true);
        let mut cfg = edited.config().clone();
        cfg.ptz = common::monitor::PtzConfig::new(
            true,
            "http://cam:8899/onvif/device_service".to_owned(),
        );
        *manager.config.lock().unwrap() = Some(MonitorConfig::new(
            cfg,
            edited.source().clone(),
            serde_json::json!({}),
        ));
        let resp = ptz_handler(State(state), query(), Json(cmd("stop"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        assert_eq!(2, *calls.lock().unwrap());
    }
}
