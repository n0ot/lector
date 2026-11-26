//! Decode and mix the embedded sound cues as 16-bit mono PCM.

use std::{io::Cursor, sync::Arc};

use hound::{SampleFormat, WavReader, WavSpec};

use crate::{Error, protocol::SoundCue};

pub(crate) const SAMPLE_RATE: u32 = 44_100;

const ONSET_CUES: [SoundCue; 5] = [
    SoundCue::Top,
    SoundCue::Bottom,
    SoundCue::Left,
    SoundCue::Right,
    SoundCue::LinkExit,
];

fn decode(cue: SoundCue) -> Result<Arc<[i16]>, Error> {
    let mut wave = WavReader::new(Cursor::new(cue.wav()))?;
    if wave.spec()
        != (WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        })
    {
        return Err(Error::OperationFailed("decode Windows sound cue format"));
    }
    let samples = wave.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
    if samples.is_empty() {
        return Err(Error::OperationFailed("decode empty Windows sound cue"));
    }
    Ok(samples.into())
}

fn mix(first: &[i16], second: &[i16]) -> Arc<[i16]> {
    let clip: Vec<i32> = (0..first.len().max(second.len()))
        .map(|index| {
            i32::from(first.get(index).copied().unwrap_or_default())
                + i32::from(second.get(index).copied().unwrap_or_default())
        })
        .collect();
    let full_scale = i32::from(i16::MAX);
    let full_range = i32::from(i16::MIN)..=full_scale;
    let peak = clip.iter().map(|sample| sample.abs()).max().unwrap_or(0);
    let overload = clip.iter().any(|sample| !full_range.contains(sample));
    clip.into_iter()
        .map(|sample| {
            let guarded = if overload {
                sample * full_scale / peak
            } else {
                sample
            };
            i16::try_from(guarded).expect("PCM gain keeps samples in range")
        })
        .collect()
}

#[derive(Debug)]
pub(crate) struct Clips {
    single: Vec<(SoundCue, Arc<[i16]>)>,
    direction_and_blank: Vec<(SoundCue, Arc<[i16]>)>,
    direction_and_link: Vec<(SoundCue, Arc<[i16]>)>,
}

impl Clips {
    pub(crate) fn new() -> Result<Self, Error> {
        let single = [
            SoundCue::Top,
            SoundCue::Bottom,
            SoundCue::Left,
            SoundCue::Right,
            SoundCue::Blank,
            SoundCue::Link,
            SoundCue::LinkExit,
        ]
        .into_iter()
        .map(|cue| Ok((cue, decode(cue)?)))
        .collect::<Result<Vec<_>, Error>>()?;
        let pairs = |other| {
            let other = Self::find(&single, other)?;
            ONSET_CUES
                .into_iter()
                .map(|direction| Ok((direction, mix(&Self::find(&single, direction)?, &other))))
                .collect::<Result<Vec<_>, Error>>()
        };
        let direction_and_blank = pairs(SoundCue::Blank)?;
        let direction_and_link = pairs(SoundCue::Link)?;
        Ok(Self {
            single,
            direction_and_blank,
            direction_and_link,
        })
    }

    fn find(clips: &[(SoundCue, Arc<[i16]>)], cue: SoundCue) -> Result<Arc<[i16]>, Error> {
        clips
            .iter()
            .find(|(name, _)| *name == cue)
            .map(|(_, clip)| Arc::clone(clip))
            .ok_or(Error::OperationFailed("select Windows sound cue"))
    }

    pub(crate) fn select(&self, cues: &[SoundCue]) -> Result<Arc<[i16]>, Error> {
        match cues {
            [cue] => Self::find(&self.single, *cue),
            [direction, SoundCue::Blank]
                if direction.is_direction() || *direction == SoundCue::LinkExit =>
            {
                Self::find(&self.direction_and_blank, *direction)
            }
            [direction, SoundCue::Link] if direction.is_direction() => {
                Self::find(&self.direction_and_link, *direction)
            }
            _ => Err(Error::OperationFailed("select Windows sound cue mix")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_and_blank_start_together_and_preserve_the_longer_tail() {
        let mixed = mix(&[2_000, -3_000, 4_000], &[700, 500]);
        assert_eq!(&*mixed, &[2_700, -2_500, 4_000]);
    }

    #[test]
    fn mix_reduces_all_samples_equally_when_the_sum_overloads() {
        let mixed = mix(&[30_000, -20_000, 10_000], &[30_000, -10_000]);
        assert_eq!(&*mixed, &[i16::MAX, -16_383, 5_461]);
    }

    #[test]
    fn quiet_overlay_does_not_normalize_a_valid_negative_full_scale_sample() {
        assert_eq!(&*mix(&[i16::MIN, 123], &[0]), &[i16::MIN, 123]);
    }

    #[test]
    fn approved_assets_prepare_without_audio_device() {
        let clips = Clips::new().unwrap();
        let blank = clips.select(&[SoundCue::Blank]).unwrap();
        for direction in ONSET_CUES {
            let bumper = clips.select(&[direction]).unwrap();
            let pair = clips.select(&[direction, SoundCue::Blank]).unwrap();
            assert_eq!(pair.len(), bumper.len().max(blank.len()));
            for (index, sample) in pair.iter().enumerate() {
                let sum = i32::from(bumper.get(index).copied().unwrap_or_default())
                    + i32::from(blank.get(index).copied().unwrap_or_default());
                assert_eq!(i32::from(*sample), sum);
                assert!(sum.abs() < i32::from(i16::MAX));
            }
        }
        let directions = clips.select(&[SoundCue::Left]).unwrap();
        let link = clips.select(&[SoundCue::Link]).unwrap();
        let linked = clips.select(&[SoundCue::Left, SoundCue::Link]).unwrap();
        assert!(
            linked
                .iter()
                .zip(directions.iter().zip(&*link))
                .all(|(&combined, (&direction, &link))| combined == direction + link)
        );
        assert_eq!(linked[link.len()..], directions[link.len()..]);
    }
}
