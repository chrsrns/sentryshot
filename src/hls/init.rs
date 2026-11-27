use crate::error::GenerateInitError;
use bytes::Bytes;
use common::TrackParameters;

#[allow(clippy::module_name_repetitions)]
pub fn generate_init(params: &TrackParameters) -> Result<Bytes, GenerateInitError> {
    Ok(Bytes::from(mp4::generate_init(
        params.width,
        params.height,
        params.extra_data.clone(),
        &params.codec,
    )?))
}
