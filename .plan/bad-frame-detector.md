## Bad-Frame Detection — Implementation Plan

### Aggressiveness control
Single global knob: **3σ (conservative) / 2σ (moderate) / 1σ (aggressive)**, applied as the rejection threshold to every metric's z-score. All z-scores computed the same way regardless of which factor:

```
z = (value - session_median) / (1.4826 × session_MAD)
```

Session-relative, not absolute — this automatically absorbs known per-session baselines (e.g. your mount's periodic error contributing to normal eccentricity variance) without hardcoding anything target- or equipment-specific. One-tailed: only the direction that means "worse" counts toward rejection.

### Three independent factors, selectable in any combination

Each factor is a boolean toggle. A frame is rejected if **any enabled factor** trips (OR logic across factors) — reasoning: clouds, focus drift, and tracking failure are physically distinct failure modes, and you want to catch any one of them, not require all three to agree.

**Factor 1 — Floor / star count (transparency)**
- Background level: median or sigma-clipped mean of sky pixels (not raw frame mean)
- Star count using local-background-relative detection threshold (not fixed ADU)
- Optional: median star SNR as a continuous companion to star count
- Reject on: background rising above threshold, star count/SNR dropping below threshold

**Factor 2 — Star FWHM (focus)**
- Median FWHM/HFR across detected stars
- Reject on: FWHM rising above threshold
- Roundness (low eccentricity + high FWHM) is the signature that isolates this as focus rather than tracking

**Factor 3 — Star eccentricity (tracking)**
- Median star eccentricity across detected stars
- Reject on: eccentricity rising above threshold
- Optionally check elongation position-angle consistency across the frame to help confirm tracking vs. other causes, though this can stay a diagnostic/logged value rather than a rejection input to start

### Per-factor metric internals
- All star-based metrics (count, FWHM, eccentricity) computed from the same detection pass, on calibrated frames, with Bayer channel split first if OSC
- Floor computed via median or sigma-clipped mean; mode available as a secondary diagnostic (bimodal histogram check) but not the primary continuous tracker, since it steps/jitters
- MAD-based robust sigma throughout, not naive stdDev

### Output behavior
- Rejected frames move to a quarantine folder, not deleted outright
- Log per frame: which factor(s) tripped, and the z-score(s) that triggered it
- Optional rolling-window view (trailing 10–15 frame median vs. session baseline) as a reporting/sanity-check layer to visualize onset timing — separate from the per-frame accept/reject decision

### UI shape
Three checkboxes (Floor/StarCount, FWHM, Eccentricity) + one sigma slider (3/2/1). Any subset of factors can run together; sigma applies uniformly across whichever are active.
