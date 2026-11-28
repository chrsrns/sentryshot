use crate::{
    error::{AdjustPartDurationError, CreateSegmenterError, SegmenterWriteH264Error},
    playlist::Playlist,
    segment::Segment,
    types::IdCounter,
};
use async_trait::async_trait;
use common::{
    DynError, H264Data, VideoSample,
    monitor::H264WriterImpl,
    time::{DurationH264, H264_MILLISECOND, H264_SECOND, UnixNano},
};
use std::{collections::HashSet, sync::Arc};
use tokio_util::sync::DropGuard;

// Opaque wrapper around segmenter that will cancel the muxer when dropped.
pub struct H264Writer {
    segmenter: Segmenter,
    _guard: DropGuard,
}

impl H264Writer {
    #[must_use]
    pub fn new(segmenter: Segmenter, guard: DropGuard) -> Self {
        Self {
            segmenter,
            _guard: guard,
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    pub async fn test_write(&mut self, pts: i64, avcc: Vec<u8>, random_access: bool) {
        use common::time::DtsOffset;
        use sentryshot_padded_bytes::PaddedBytes;

        self.write_h264(H264Data {
            pts: common::time::UnixH264::new(pts),
            dts_offset: DtsOffset::new(0),
            avcc: Arc::new(PaddedBytes::new(avcc)),
            random_access_present: random_access,
        })
        .await
        .unwrap();
    }
}

#[async_trait]
impl H264WriterImpl for H264Writer {
    async fn write_h264(&mut self, data: H264Data) -> Result<(), DynError> {
        Ok(self.segmenter.write_h264(data).await?)
    }
}

pub struct Segmenter {
    segment_duration: DurationH264,
    part_duration: DurationH264,
    segment_max_size: u64,
    playlist: Arc<Playlist>,
    muxer_id: u16,

    muxer_start_time: UnixNano,
    //last_video_params: Vec<Vec<u8>>,
    current_segment: Option<Segment>,
    segment_id_counter: IdCounter,
    part_id_counter: IdCounter,
    next_sample: VideoSample,
    first_segment_finalized: bool,
    sample_durations: HashSet<DurationH264>,
    adjusted_part_duration: DurationH264,
}

impl Segmenter {
    pub fn new(
        segment_duration: DurationH264,
        part_duration: DurationH264,
        segment_max_size: u64,
        playlist: Arc<Playlist>,
        muxer_id: u16,
        muxer_start_time: UnixNano,
        first_sample: H264Data,
    ) -> Result<Self, CreateSegmenterError> {
        if !first_sample.random_access_present {
            return Err(CreateSegmenterError::NotIdr);
        }
        if *first_sample.dts_offset != 0 {
            return Err(CreateSegmenterError::DtsNotZero);
        }

        let next_sample = VideoSample {
            pts: muxer_start_time.into(),
            dts_offset: first_sample.dts_offset,
            avcc: first_sample.avcc,
            random_access_present: first_sample.random_access_present,
            duration: DurationH264::new(0),
        };

        Ok(Self {
            segment_duration,
            part_duration,
            segment_max_size,
            playlist,
            muxer_start_time,
            muxer_id,
            //last_video_params: Vec::new(),
            current_segment: None,
            segment_id_counter: IdCounter::new(7), // 7 required by iOS.
            part_id_counter: IdCounter::new(0),
            next_sample,
            first_segment_finalized: false,
            sample_durations: HashSet::new(),
            adjusted_part_duration: DurationH264::new(0),
        })
    }

    // iPhone iOS fails if part durations are less than 85% of maximum part duration.
    // find a part duration that is compatible with all received sample durations.
    fn adjust_part_duration(&mut self, du: DurationH264) -> Result<(), AdjustPartDurationError> {
        if self.first_segment_finalized {
            return Ok(());
        }

        // Skip invalid durations.
        if *du == 0 {
            return Ok(());
        }

        if !self.sample_durations.contains(&du) {
            self.sample_durations.insert(du);
            self.adjusted_part_duration =
                find_compatible_part_duration(self.part_duration, &self.sample_durations)
                    .ok_or(AdjustPartDurationError::Error)?;
        }
        Ok(())
    }

    pub async fn write_h264(&mut self, data: H264Data) -> Result<(), SegmenterWriteH264Error> {
        use crate::error::SegmenterWriteH264Error::*;

        let sample = VideoSample {
            pts: data.pts,
            dts_offset: data.dts_offset,
            avcc: data.avcc,
            random_access_present: data.random_access_present,
            duration: DurationH264::new(0),
        };

        let next_dts = sample.dts().ok_or(Dts)?;

        // Put samples in a queue in order to compute sample duration.
        let mut sample = std::mem::replace(&mut self.next_sample, sample);

        let sample_dts = sample.dts().ok_or(Dts)?;

        sample.duration = next_dts
            .checked_sub(sample_dts)
            .ok_or(ComputeSampleDuration)?
            .into();
        if *sample.duration < 0 {
            sample.duration = DurationH264::new(0);
        }

        self.adjust_part_duration(sample.duration)?;

        let current_segment = self.current_segment.get_or_insert_with(|| {
            Segment::new(
                self.segment_id_counter.next_id(),
                self.muxer_id,
                sample_dts,
                self.muxer_start_time,
                self.segment_max_size,
                self.playlist.clone(),
                &mut self.part_id_counter,
            )
        });

        let segment_start_dts = current_segment.start_dts();
        current_segment
            .write_h264(
                sample,
                self.adjusted_part_duration,
                &mut self.part_id_counter,
            )
            .await?;

        // switch segment
        if data.random_access_present {
            if DurationH264::from(
                next_dts
                    .checked_sub(segment_start_dts)
                    .ok_or(SwitchSegment)?,
            ) >= self.segment_duration
            {
                let next_segment = Segment::new(
                    self.segment_id_counter.next_id(),
                    self.muxer_id,
                    sample_dts,
                    self.muxer_start_time,
                    self.segment_max_size,
                    self.playlist.clone(),
                    &mut self.part_id_counter,
                );
                let prev_segment = std::mem::replace(current_segment, next_segment);

                let Some(finalized_segment) = prev_segment.finalize(next_dts).await? else {
                    // Cancelled.
                    return Ok(());
                };
                self.playlist.on_segment_finalized(finalized_segment).await;

                self.first_segment_finalized = true;

            }
        }

        Ok(())
    }
}



fn part_duration_is_compatible(
    part_duration: DurationH264,
    sample_duration: DurationH264,
) -> Option<bool> {
    if sample_duration > part_duration {
        return Some(false);
    }

    let mut f = part_duration.checked_div(sample_duration)?;
    if !(part_duration.checked_rem(sample_duration)?).is_zero() {
        f = f.checked_add(DurationH264::new(1))?;
    }
    f = f.checked_mul(sample_duration)?;

    Some(
        part_duration
            > f.checked_mul(DurationH264::new(85))?
                .checked_div(DurationH264::new(100))?,
    )
}

fn part_duration_is_compatible_with_all(
    part_duration: DurationH264,
    sample_durations: &HashSet<DurationH264>,
) -> Option<bool> {
    for sd in sample_durations {
        if !part_duration_is_compatible(part_duration, *sd)? {
            return Some(false);
        }
    }
    Some(true)
}

fn find_compatible_part_duration(
    min_part_duration: DurationH264,
    sample_durations: &HashSet<DurationH264>,
) -> Option<DurationH264> {
    let mut i = min_part_duration;
    while *i < 5 * H264_SECOND {
        if part_duration_is_compatible_with_all(i, sample_durations)? {
            break;
        }
        i = i.checked_add(DurationH264::new(5 * H264_MILLISECOND))?;
    }
    Some(i)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        DummyLogger,
        time::{DtsOffset, UnixH264},
    };
    use sentryshot_padded_bytes::PaddedBytes;
    use tokio_util::sync::CancellationToken;

    fn frame(pts: i64, random_access: bool) -> H264Data {
        H264Data {
            pts: UnixH264::new(pts),
            dts_offset: DtsOffset::new(0),
            avcc: Arc::new(PaddedBytes::new(vec![1])),
            random_access_present: random_access,
        }
    }

    fn test_playlist() -> Arc<Playlist> {
        Arc::new(Playlist::new(
            CancellationToken::new(),
            DummyLogger::new(),
            10,
            0,
        ))
    }

    #[test]
    fn part_duration_is_compatible_basic() {
        // Sample duration larger than part duration.
        assert_eq!(
            Some(false),
            part_duration_is_compatible(DurationH264::new(90_000), DurationH264::new(90_001))
        );
        // Divides evenly: always compatible.
        assert_eq!(
            Some(true),
            part_duration_is_compatible(DurationH264::new(90_000), DurationH264::new(45_000))
        );
        // Non-divisible: ceil multiple must be >= part_duration/0.85.
        assert_eq!(
            Some(true),
            part_duration_is_compatible(DurationH264::new(90_000), DurationH264::new(46_000))
        );
        assert_eq!(
            Some(false),
            part_duration_is_compatible(DurationH264::new(90_000), DurationH264::new(89_000))
        );
        // Zero sample duration -> None.
        assert_eq!(
            None,
            part_duration_is_compatible(DurationH264::new(90_000), DurationH264::new(0))
        );
    }

    #[test]
    fn find_compatible_part_duration_basic() {
        // Empty set: minimum is returned as-is.
        assert_eq!(
            Some(DurationH264::new(90_000)),
            find_compatible_part_duration(DurationH264::new(90_000), &HashSet::new())
        );
        // Compatible minimum is not bumped.
        assert_eq!(
            Some(DurationH264::new(90_000)),
            find_compatible_part_duration(
                DurationH264::new(90_000),
                &HashSet::from([DurationH264::new(45_000)])
            )
        );
        // sd=60000: first i >= min where 2*60000*0.85 < i is 90000+27*450.
        assert_eq!(
            Some(DurationH264::new(102_150)),
            find_compatible_part_duration(
                DurationH264::new(90_000),
                &HashSet::from([DurationH264::new(60_000)])
            )
        );
    }

    #[tokio::test]
    async fn segmenter_new_rejects_non_idr() {
        let got = Segmenter::new(
            DurationH264::new(H264_SECOND),
            DurationH264::new(H264_SECOND / 3),
            1_000_000,
            test_playlist(),
            0,
            UnixNano::new(0),
            frame(0, false),
        );
        assert!(matches!(
            got,
            Err(crate::error::CreateSegmenterError::NotIdr)
        ));
    }

    #[tokio::test]
    async fn segmenter_new_rejects_nonzero_dts() {
        let mut first = frame(0, true);
        first.dts_offset = DtsOffset::new(1);
        let got = Segmenter::new(
            DurationH264::new(H264_SECOND),
            DurationH264::new(H264_SECOND / 3),
            1_000_000,
            test_playlist(),
            0,
            UnixNano::new(0),
            first,
        );
        assert!(matches!(
            got,
            Err(crate::error::CreateSegmenterError::DtsNotZero)
        ));
    }

    #[tokio::test]
    async fn adjust_part_duration_edges() {
        let mut s = Segmenter::new(
            DurationH264::new(H264_SECOND),
            DurationH264::new(H264_SECOND / 3),
            1_000_000,
            test_playlist(),
            0,
            UnixNano::new(0),
            frame(0, true),
        )
        .unwrap();

        // Zero durations are skipped entirely.
        s.adjust_part_duration(DurationH264::new(0)).unwrap();
        assert!(s.sample_durations.is_empty());

        // New durations get recorded.
        s.adjust_part_duration(DurationH264::new(30_000)).unwrap();
        assert!(s.sample_durations.contains(&DurationH264::new(30_000)));

        // Once the first segment is finalized, durations freeze.
        s.first_segment_finalized = true;
        s.adjust_part_duration(DurationH264::new(60_000)).unwrap();
        assert!(!s.sample_durations.contains(&DurationH264::new(60_000)));
    }

    #[tokio::test]
    async fn segment_switch_start_dts_characterization() {
        // B38: the new segment is anchored on the previous sample's dts,
        // not the incoming IDR's dts. This test characterizes that
        // reported start_time/duration are off by one frame interval.
        let playlist = test_playlist();
        let mut s = Segmenter::new(
            DurationH264::new(90_000),
            DurationH264::new(30_000),
            1_000_000,
            playlist.clone(),
            0,
            UnixNano::new(0),
            frame(0, true),
        )
        .unwrap();

        // Segments switch when an IDR arrives at least segment_duration
        // after the segment start.
        s.write_h264(frame(45_000, false)).await.unwrap();
        s.write_h264(frame(90_000, false)).await.unwrap();
        s.write_h264(frame(200_000, true)).await.unwrap();

        assert!(s.first_segment_finalized);
        // Actual behavior (B38): start_dts = last frame of old segment.
        // Correct value would be 200_000 (the incoming IDR's dts).
        assert_eq!(UnixH264::new(90_000), s.current_segment.as_ref().unwrap().start_dts());
        assert_eq!(1, playlist.debug_state().await.num_segments);
    }

    #[tokio::test]
    async fn segmenter_write_rejects_over_max_size() {
        let mut s = Segmenter::new(
            DurationH264::new(H264_SECOND),
            DurationH264::new(H264_SECOND / 3),
            2, // segment_max_size bytes
            test_playlist(),
            0,
            UnixNano::new(0),
            frame(0, true),
        )
        .unwrap();

        let mut big = frame(45_000, false);
        big.avcc = Arc::new(PaddedBytes::new(vec![0; 64]));
        // Samples are written one call late; the big sample lands on the
        // next write and blows the 2-byte segment budget.
        s.write_h264(big).await.unwrap();
        let got = s.write_h264(frame(90_000, false)).await;
        assert!(matches!(
            got,
            Err(crate::error::SegmenterWriteH264Error::WriteH264(
                crate::error::PartWriteH264Error::MaximumSegmentSize
            ))
        ));
    }
}
