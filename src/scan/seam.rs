// SPDX-License-Identifier: GPL-2.0-or-later

//! `OnvifClient` seam — the swap point over `onvif-device-rs`.
//! Consumers see seam types only, never dep types.

use crate::{
    ScanError,
    types::{Creds, DiscoveredDevice, Profile, Services},
};
use async_trait::async_trait;
use std::time::Duration;

/// ONVIF client abstraction. Concrete impl wraps `onvif-device-rs`.
#[async_trait]
pub trait OnvifClient: Send + Sync {
    /// WS-Discovery probe + collection. `window` is the reply window.
    async fn discover(&self, window: Duration) -> Result<Vec<DiscoveredDevice>, ScanError>;

    /// `GetSystemDateAndTime` — needed for WSSE clock-offset computation.
    async fn device_time(&self, xaddr: &str, creds: &Creds) -> Result<jiff::Timestamp, ScanError>;

    /// `GetServices`/`GetCapabilities` — service endpoints + capability flags.
    async fn services(&self, xaddr: &str, creds: &Creds) -> Result<Services, ScanError>;

    /// `GetProfiles` on the media service.
    async fn profiles(&self, media_xaddr: &str, creds: &Creds) -> Result<Vec<Profile>, ScanError>;

    /// `GetStreamUri` for a profile token.
    async fn stream_uri(
        &self,
        media_xaddr: &str,
        token: &str,
        creds: &Creds,
    ) -> Result<String, ScanError>;
}
