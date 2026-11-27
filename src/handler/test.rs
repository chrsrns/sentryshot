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
        saved: Mutex<Vec<MonitorConfig>>,
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
            config: MonitorConfig,
        ) -> Option<Result<bool, MonitorSetError>> {
            *self.config.lock().unwrap() = Some(config.clone());
            self.saved.lock().unwrap().push(config);
            Some(Ok(false))
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
        async fn monitor_unmark_audio(
            &self,
            _id: MonitorId,
            names: Vec<String>,
        ) -> Option<Result<(), MonitorSetError>> {
            let mut cfg = self.config.lock().unwrap();
            if let Some(c) = cfg.as_mut() {
                let rem: Vec<String> = c
                    .config()
                    .audio
                    .unsupported()
                    .iter()
                    .filter(|m| !names.contains(m))
                    .cloned()
                    .collect();
                if let Some(a) = c
                    .raw_mut()
                    .get_mut("audio")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    a.insert("unsupported".to_owned(), serde_json::json!(rem));
                }
                if let Ok(f) = serde_json::from_value::<MonitorConfig>(c.raw().clone()) {
                    *c = f;
                }
            }
            Some(Ok(()))
        }
    }

    struct StubPtz {
        err: bool,
        presets: Vec<ptz::Preset>,
        // op names that fault `not implemented` (B109 camera shape).
        not_impl: std::collections::HashSet<&'static str>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ptz::PtzClient for StubPtz {
        async fn move_cam(
            &self,
            _pan: Option<f64>,
            _tilt: Option<f64>,
            _zoom: Option<f64>,
            _timeout: Option<&str>,
        ) -> Result<(), ptz::Error> {
            if self.err {
                return Err(ptz::Error::NoPtzService);
            }
            Ok(())
        }
        async fn stop(&self) -> Result<(), ptz::Error> {
            self.calls.lock().unwrap().push("stop");
            if self.not_impl.contains("stop") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok(())
        }
        async fn home(&self) -> Result<(), ptz::Error> {
            self.calls.lock().unwrap().push("home");
            if self.not_impl.contains("home") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok(())
        }
        async fn get_presets(&self) -> Result<Vec<ptz::Preset>, ptz::Error> {
            self.calls.lock().unwrap().push("getPresets");
            if self.not_impl.contains("getPresets") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok(self.presets.clone())
        }
        async fn set_preset(&self, _name: &str) -> Result<String, ptz::Error> {
            self.calls.lock().unwrap().push("setPreset");
            if self.not_impl.contains("setPreset") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok("1".into())
        }
        async fn goto_preset(&self, _token: &str) -> Result<(), ptz::Error> {
            self.calls.lock().unwrap().push("gotoPreset");
            if self.not_impl.contains("gotoPreset") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok(())
        }
        async fn remove_preset(&self, _token: &str) -> Result<(), ptz::Error> {
            self.calls.lock().unwrap().push("removePreset");
            if self.not_impl.contains("removePreset") {
                return Err(ptz::Error::NotImplemented);
            }
            Ok(())
        }
    }

    fn cmd(command: &str) -> ptz::PtzCommand {
        ptz::PtzCommand {
            command: command.to_owned(),
            pan: None,
            tilt: None,
            zoom: None,
            timeout: None,
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
                audio: common::monitor::AudioConfig::default(),
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
                    saved: Mutex::new(vec![]),
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
            not_impl: std::collections::HashSet::new(),
            calls: Arc::new(Mutex::new(vec![])),
        })
    }

    /// Monitor config carrying persisted capability marks — built
    /// through serde so typed `ptz.unsupported` reflects raw.
    fn monitor_config_marks(marks: &[&str]) -> MonitorConfig {
        serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "m", "enable": true,
            "source": "rtsp",
            "sourcertsp": {
                "protocol": "tcp",
                "mainStream": "rtsp://u%40x:p%40ss@cam/main",
            },
            "alwaysRecord": false, "videoLength": 0.0,
            "ptz": {"enable": true, "onvifAddr": "", "unsupported": marks},
        }))
        .unwrap()
    }

    /// Monitor config with a raw `ptz` object (persist target) and no
    /// marks.
    fn monitor_config_ptz() -> MonitorConfig {
        monitor_config_marks(&[])
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
    async fn ptz_timeout_outside_allowlist_is_400() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(false));
        let mut c = cmd("move");
        c.pan = Some(0.5);
        c.timeout = Some("PT3S".to_owned());
        let resp = ptz_handler(state, query, Json(c)).await;
        assert_eq!(StatusCode::BAD_REQUEST, resp.status());
    }

    #[tokio::test]
    async fn ptz_timeout_on_non_move_is_400() {
        let (state, query, _) = harness(Some(monitor_config(true)), stub_client(false));
        let mut c = cmd("stop");
        c.timeout = Some("PT0.5S".to_owned());
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
            not_impl: std::collections::HashSet::new(),
            calls: Arc::new(Mutex::new(vec![])),
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
            _timeout: Option<&str>,
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
            saved: Mutex::new(vec![]),
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

    #[tokio::test]
    async fn capabilities_returns_persisted_marks() {
        let calls = Arc::new(Mutex::new(vec![]));
        let stub = Arc::new(StubPtz {
            err: false,
            presets: vec![],
            not_impl: std::collections::HashSet::new(),
            calls: calls.clone(),
        });
        let (state, query, _) = harness(Some(monitor_config_marks(&["home", "stop"])), stub);
        let resp = ptz_handler(state, query, Json(cmd("capabilities"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["unsupported"], serde_json::json!(["home", "stop"]));
        // Eager probe ran — getPresets answered OK, no mark added.
        assert_eq!(vec!["getPresets"], *calls.lock().unwrap());
    }

    #[tokio::test]
    async fn capabilities_probe_marks_and_persists() {
        let calls = Arc::new(Mutex::new(vec![]));
        let stub = Arc::new(StubPtz {
            err: false,
            presets: vec![],
            not_impl: std::collections::HashSet::from(["getPresets"]),
            calls: calls.clone(),
        });
        let manager = Arc::new(StubManager {
            config: Mutex::new(Some(monitor_config_ptz())),
            saved: Mutex::new(vec![]),
        });
        let state = Arc::new(PtzRouteState {
            monitors: manager.clone(),
            service: PtzService::new(Arc::new(move |_e, _c| stub.clone())),
        });
        let query = || {
            Query(MonitorIdQuery {
                id: "m1".to_owned().try_into().unwrap(),
            })
        };
        let st = State(state.clone());
        let resp = ptz_handler(st, query(), Json(cmd("capabilities"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["unsupported"], serde_json::json!(["getPresets"]));

        // Persisted into stored config — and a second call reads the
        // mark without re-probing the camera.
        let saved = manager.saved.lock().unwrap();
        assert_eq!(1, saved.len());
        assert_eq!(&["getPresets"], saved[0].config().ptz.unsupported());
        drop(saved);
        let resp = ptz_handler(State(state), query(), Json(cmd("capabilities"))).await;
        assert_eq!(StatusCode::OK, resp.status());
        assert_eq!(vec!["getPresets"], *calls.lock().unwrap());
    }

    #[tokio::test]
    async fn marked_op_short_circuits_502() {
        let calls = Arc::new(Mutex::new(vec![]));
        let stub = Arc::new(StubPtz {
            err: false,
            presets: vec![],
            not_impl: std::collections::HashSet::new(),
            calls: calls.clone(),
        });
        let (state, query, _) = harness(Some(monitor_config_marks(&["home"])), stub);
        let resp = ptz_handler(state, query, Json(cmd("home"))).await;
        assert_eq!(StatusCode::BAD_GATEWAY, resp.status());
        // ⊥ camera round trip — persisted mark did the answering.
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn op_fault_marks_and_short_circuits_next() {
        let calls = Arc::new(Mutex::new(vec![]));
        let stub = Arc::new(StubPtz {
            err: false,
            presets: vec![],
            not_impl: std::collections::HashSet::from(["home"]),
            calls: calls.clone(),
        });
        let manager = Arc::new(StubManager {
            config: Mutex::new(Some(monitor_config_ptz())),
            saved: Mutex::new(vec![]),
        });
        let state = Arc::new(PtzRouteState {
            monitors: manager.clone(),
            service: PtzService::new(Arc::new(move |_e, _c| stub.clone())),
        });
        let query = || {
            Query(MonitorIdQuery {
                id: "m1".to_owned().try_into().unwrap(),
            })
        };
        let st = State(state.clone());
        // First call — camera faults → marked + persisted.
        let resp = ptz_handler(st, query(), Json(cmd("home"))).await;
        assert_eq!(StatusCode::BAD_GATEWAY, resp.status());
        assert_eq!(vec!["home"], *calls.lock().unwrap());
        let saved = manager.saved.lock().unwrap();
        assert_eq!(1, saved.len());
        assert_eq!(&["home"], saved[0].config().ptz.unsupported());
        drop(saved);

        // Second call — persisted mark short-circuits, ⊥ camera call.
        let resp = ptz_handler(State(state), query(), Json(cmd("home"))).await;
        assert_eq!(StatusCode::BAD_GATEWAY, resp.status());
        assert_eq!(vec!["home"], *calls.lock().unwrap());
    }
}

// SPDX-SnippetBegin
// SPDX-Snippet-CopyrightText: two-way audio route tests
#[cfg(test)]
mod audio_tests {
    use crate::{
        AudioRouteState, MonitorIdQuery, audio_session_handler, busy_ctrl, close_frame,
        origin_matches_host,
    };
    use async_trait::async_trait;
    use axum::extract::{Query, State};
    use common::{
        MonitorId,
        monitor::{MonitorConfig, MonitorConfigs, MonitorSetError},
    };
    use http::{HeaderMap, HeaderValue, StatusCode, header};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    struct StubManager {
        config: Mutex<Option<MonitorConfig>>,
        saved: Arc<Mutex<Vec<MonitorConfig>>>,
    }

    #[async_trait]
    impl common::monitor::IMonitorManager for StubManager {
        async fn monitor_restart(
            &self,
            _id: MonitorId,
        ) -> Option<Result<(), common::monitor::MonitorRestartError>> {
            unimplemented!()
        }
        async fn monitor_set(
            &self,
            config: MonitorConfig,
        ) -> Option<Result<bool, MonitorSetError>> {
            *self.config.lock().unwrap() = Some(config.clone());
            self.saved.lock().unwrap().push(config);
            Some(Ok(false))
        }
        async fn monitor_set_and_restart(
            &self,
            _c: MonitorConfig,
        ) -> Option<Result<bool, common::monitor::MonitorSetAndRestartError>> {
            unimplemented!()
        }
        async fn monitor_delete(
            &self,
            _id: MonitorId,
        ) -> Option<Result<(), common::monitor::MonitorDeleteError>> {
            unimplemented!()
        }
        async fn monitors_info(&self) -> Option<HashMap<MonitorId, common::monitor::MonitorInfo>> {
            unimplemented!()
        }
        async fn monitor_config(&self, _id: MonitorId) -> Option<Option<MonitorConfig>> {
            Some(self.config.lock().unwrap().clone())
        }
        async fn monitor_configs(&self) -> Option<MonitorConfigs> {
            unimplemented!()
        }
        async fn monitor_unmark_audio(
            &self,
            _id: MonitorId,
            names: Vec<String>,
        ) -> Option<Result<(), MonitorSetError>> {
            let mut cfg = self.config.lock().unwrap();
            if let Some(c) = cfg.as_mut() {
                let rem: Vec<String> = c
                    .config()
                    .audio
                    .unsupported()
                    .iter()
                    .filter(|m| !names.contains(m))
                    .cloned()
                    .collect();
                if let Some(a) = c
                    .raw_mut()
                    .get_mut("audio")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    a.insert("unsupported".to_owned(), serde_json::json!(rem));
                }
                if let Ok(f) = serde_json::from_value::<MonitorConfig>(c.raw().clone()) {
                    *c = f;
                }
            }
            Some(Ok(()))
        }
    }

    fn monitor_config_audio(enable: bool, marks: &[&str]) -> MonitorConfig {
        serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "m", "enable": true,
            "source": "rtsp",
            "sourcertsp": {
                "protocol": "tcp",
                "mainStream": "rtsp://u%40x:p%40ss@cam/main",
            },
            "alwaysRecord": false, "videoLength": 0.0,
            "ptz": {"enable": false, "onvifAddr": ""},
            "audio": {"enable": enable, "unsupported": marks},
        }))
        .unwrap()
    }

    /// The stub session never actually opens — attach paths that
    /// reach `open` would hang, so route tests only exercise the
    /// pre-attach surface (token mint + statuses).
    fn service() -> (
        Arc<audio::AudioService>,
        Arc<Mutex<Vec<(MonitorId, Vec<String>)>>>,
    ) {
        let persisted = Arc::new(Mutex::new(Vec::<(MonitorId, Vec<String>)>::new()));
        let p = persisted.clone();
        (
            Arc::new(audio::AudioService::new(
                Arc::new(|url, alt, cfg| {
                    Arc::new(audio::RtspAudioClient::new(url, alt, cfg)) as audio::ArcAudioClient
                }),
                Arc::new(move |id: MonitorId, marks: Vec<String>| {
                    let p = p.clone();
                    Box::pin(async move {
                        p.lock().unwrap().push((id, marks));
                    })
                }),
                Arc::new(|_id: MonitorId, _names: Vec<String>| Box::pin(async move {})),
            )),
            persisted,
        )
    }

    fn harness(
        config: Option<MonitorConfig>,
    ) -> (
        State<Arc<AudioRouteState>>,
        Query<MonitorIdQuery>,
        Arc<Mutex<Vec<MonitorConfig>>>,
    ) {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (service, _) = service();
        (
            State(Arc::new(AudioRouteState {
                monitors: Arc::new(StubManager {
                    config: Mutex::new(config),
                    saved: saved.clone(),
                }),
                service,
            })),
            Query(MonitorIdQuery {
                id: "m1".to_owned().try_into().unwrap(),
            }),
            saved,
        )
    }

    #[tokio::test]
    async fn audio_session_missing_monitor_is_404() {
        let (state, query, _) = harness(None);
        let resp = audio_session_handler(state, query).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audio_session_disabled_is_409() {
        let (state, query, _) = harness(Some(monitor_config_audio(false, &[])));
        let resp = audio_session_handler(state, query).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn audio_session_all_marked_is_409() {
        let (state, query, _) = harness(Some(monitor_config_audio(true, &["listen", "talk"])));
        let resp = audio_session_handler(state, query).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn audio_session_partial_marks_mints_token() {
        let (state, query, _) = harness(Some(monitor_config_audio(true, &["talk"])));
        let resp = audio_session_handler(state, query).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    #[tokio::test]
    async fn audio_session_enabled_mints_single_use_token() {
        let (state, query, _) = harness(Some(monitor_config_audio(true, &[])));
        let resp = audio_session_handler(state.clone(), query).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let service = &state.0.service;
        // Minted tokens resolve once.
        let token = service
            .mint_token("m1".to_owned().try_into().unwrap())
            .await;
        assert!(service.consume_token(&token).await.is_some());
        assert!(service.consume_token(&token).await.is_none());
    }

    #[test]
    fn origin_host_match() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("cam.local:8080"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://cam.local:8080"),
        );
        assert!(origin_matches_host(&headers));
    }

    #[test]
    fn origin_host_default_port() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("cam.local"));
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://cam.local"));
        assert!(origin_matches_host(&headers));
    }

    #[test]
    fn origin_mismatch_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("cam.local:8080"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example:8080"),
        );
        assert!(!origin_matches_host(&headers));
    }

    #[test]
    fn origin_port_mismatch_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("cam.local:8080"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://cam.local:9090"),
        );
        assert!(!origin_matches_host(&headers));
    }

    #[test]
    fn origin_absent_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("cam.local:8080"));
        assert!(!origin_matches_host(&headers));
    }

    #[test]
    fn busy_ctrl_carries_the_denied_verb() {
        let axum::extract::ws::Message::Text(text) = busy_ctrl() else {
            panic!("busy must be a text ctrl");
        };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "busy");
        assert_eq!(v["command"], "talk-start");
    }

    #[test]
    fn close_frame_carries_code() {
        let axum::extract::ws::Message::Close(Some(frame)) = close_frame(1008) else {
            panic!("expected close frame");
        };
        assert_eq!(frame.code, 1008);
    }

    #[test]
    fn audio_entry_key_covers_sub_stream() {
        let base = monitor_config_audio(true, &[]);
        let k1 = crate::audio_key(&base);
        assert!(!k1.contains('|'), "{k1}");

        // Same mainStream, different subStream → different key —
        // the 429-retry URL must force an entry rebuild.
        let with_sub: MonitorConfig = serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "m", "enable": true,
            "source": "rtsp",
            "sourcertsp": {
                "protocol": "tcp",
                "mainStream": "rtsp://u%40x:p%40ss@cam/main",
                "subStream": "rtsp://u%40x:p%40ss@cam/sub",
            },
            "alwaysRecord": false, "videoLength": 0.0,
            "ptz": {"enable": false, "onvifAddr": ""},
            "audio": {"enable": true, "unsupported": []},
        }))
        .unwrap();
        let k2 = crate::audio_key(&with_sub);
        assert_ne!(k1, k2);
        assert!(k2.contains("sub"), "{k2}");
    }
}
// SPDX-SnippetEnd
