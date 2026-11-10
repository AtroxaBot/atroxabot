//! WAV encoding for speech-to-text uploads.

/// Wraps 16-bit mono PCM in a WAV container.
pub(crate) fn encode_wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len() * 2).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44 + pcm.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&data_len.saturating_add(36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // size of the fmt chunk
    wav.extend_from_slice(&1u16.to_le_bytes()); // integer PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&rate.saturating_mul(2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend(pcm.iter().flat_map(|sample| sample.to_le_bytes()));
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
    }

    #[test]
    fn encode_wav_round_trip() {
        let pcm: Vec<i16> = (-50..50).collect();
        let wav = encode_wav(&pcm, 16_000);
        assert_eq!(wav.len(), 44 + 200);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32_at(&wav, 4), 36 + 200);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&wav, 16), 16);
        assert_eq!((u16_at(&wav, 20), u16_at(&wav, 22)), (1, 1));
        assert_eq!((u32_at(&wav, 24), u32_at(&wav, 28)), (16_000, 32_000));
        assert_eq!((u16_at(&wav, 32), u16_at(&wav, 34)), (2, 16));
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32_at(&wav, 40), 200);
        let decoded: Vec<i16> = wav[44..].as_chunks::<2>().0.iter().map(|&pair| i16::from_le_bytes(pair)).collect();
        assert_eq!(decoded, pcm);
    }

    #[test]
    fn empty_audio_is_a_bare_header() {
        let wav = encode_wav(&[], 16_000);
        assert_eq!(wav.len(), 44);
        assert_eq!(u32_at(&wav, 40), 0);
    }
}
