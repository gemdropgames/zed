//! Off-thread half of opening a file.
//!
//! The daemon decodes, buckets and sizes: [`probe`] is one
//! `ggo_audio_probe` round trip that answers everything the tab shows --
//! rate, channels, duration, the waveform outline, and (for a `.adp`) the
//! file's own bytes, which ARE its baked form.
//!
//! **One local decode survives, and only for a source file.** A `.adp`
//! previews straight from its blob, so opening one touches no codec here
//! at all. A `.wav`/`.ogg` in Source mode feeds raw PCM to the `Apu`, and
//! raw PCM is the one thing the probe deliberately does not send -- see
//! `docs/daemon-api-plan.md` §4.4. That decode goes in P4, when the
//! preview moves onto the shm audio ring.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ggo_audio::Decoded;
use ggo_daemon_client::{AudioBudget, AudioProbe, Connect};

pub struct Loaded {
    /// The clip's shape and outline, as the daemon reported it.
    pub probe: AudioProbe,
    /// What a baked blob costs, and the rates the tab may offer. For a
    /// `.adp` this is its own cost; for a source file it is zero bytes
    /// and the rate list, until the first bake lands.
    pub budget: AudioBudget,
    /// PCM for the Source-mode preview. `None` for a `.adp`, which
    /// previews from its blob and needs no samples.
    pub decoded: Option<Arc<Decoded>>,
    /// For a `.adp`: the file itself. `None` for a source file, whose
    /// bake the panel asks for separately at the chosen rate.
    pub adp: Option<Arc<Vec<u8>>>,
}

/// **Blocking** (two socket round trips), so callers stay off the UI
/// thread -- the panel runs this inside `cx.background_spawn`.
pub fn load(connect: &Connect, path: &Path) -> Result<Loaded> {
    let client = connect().context("the GemdropGo daemon is unavailable")?;
    let probe = client
        .audio_probe(&path.to_string_lossy())
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;

    // A `.adp`'s own bytes are its baked form, so its cost is known now.
    // A source file has none yet: the empty blob asks the same tool for
    // the rate list alone, rather than this crate keeping a second copy
    // of which rates exist.
    let adp = probe.adp.clone().map(Arc::new);
    let budget = client
        .audio_budget(adp.as_deref().map(Vec::as_slice).unwrap_or(&[]))
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;

    // The one local decode, and only for what the preview will need.
    let decoded = match adp {
        Some(_) => None,
        None => Some(Arc::new(ggo_audio::decode(path)?)),
    };

    Ok(Loaded {
        probe,
        budget,
        decoded,
        adp,
    })
}
