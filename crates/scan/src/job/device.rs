//! A scanner's paper-path status as a [`craft_job::DeviceView`].

use craft_job::{
    CapabilitiesSummary, DeviceConnection, DeviceId, DeviceState, DeviceView, Indicator, IndicatorKey, JobKind, PaperPathView, PaperSource, Tri,
};

use crate::{Connection, HandlingStatus, Scanner, Source};

/// `Usb` is `Usb`, `Wsd` is `Network`, and `Other` and `Unknown` stay themselves. (`Other` is how an eSCL
/// scanner is listed: it is on the network, but the registry only says "not USB, not WSD".)
impl From<Connection> for DeviceConnection {
    fn from(connection: Connection) -> Self {
        match connection {
            Connection::Usb => Self::Usb,
            Connection::Wsd => Self::Network,
            Connection::Other => Self::Other,
            Connection::Unknown => Self::Unknown,
        }
    }
}

/// One scanner as a [`DeviceView`]: capabilities, the paper path and an indicator for each status bit.
///
/// `status` is what [`crate::status`] answered (`None`: the backend cannot preflight), or
/// `Some(scanner.handling_status)` for the reading taken with the details. Every indicator is a [`Tri`]:
///
/// * `feeder_ready` is an `Option<bool>` and is taken as the backend gave it: `None` is `NotReported`.
/// * The other bits are plain `bool`s that mean "set only when reported". A status that is entirely clear and
///   leaves the feeder unreported ([`HandlingStatus::default`]) is what a driver that reports nothing looks
///   like (the inbox WSD scan driver reports every bit clear), so **every** indicator is `NotReported`, never
///   `Off`. Once any bit says something, the others are read as given: a clear fault bit is `Off`.
/// * An indicator for hardware the scanner does not have (no feeder, no flatbed, no duplexer) is left out
///   unless the status asserts it.
///
/// `source_in_use` fills [`PaperPathView::source_in_use`] (`Auto` says nothing).
pub fn device_view(scanner: &Scanner, status: Option<HandlingStatus>, source_in_use: Option<Source>) -> DeviceView {
    let id = scanner.id.parse::<DeviceId>().unwrap_or_else(|_| DeviceId::new("unknown", scanner.id.clone()));
    let reported = status.filter(|s| *s != HandlingStatus::default());
    let flag = |bit: fn(&HandlingStatus) -> bool| reported.map_or(Tri::NotReported, |s| Tri::from_bool(bit(&s)));
    let feeder = reported.map_or(Tri::NotReported, |s| Tri::from_option(s.feeder_ready));
    let flat = flag(|s| s.flat_ready);
    let duplex = flag(|s| s.duplex_ready);

    let capabilities = CapabilitiesSummary::new()
        .with_feeder(Tri::from_bool(scanner.feeder))
        .with_flatbed(Tri::from_bool(scanner.flatbed))
        .with_duplex(Tri::from_bool(scanner.duplex));

    let mut paper = PaperPathView::new().with_feeder_loaded(feeder).with_glass_available(if scanner.flatbed { flat } else { Tri::Off });
    if let Some(capacity) = scanner.feeder_capacity {
        paper = paper.with_feeder_capacity(capacity);
    }
    match source_in_use {
        Some(Source::Feeder) => paper = paper.with_source_in_use(PaperSource::Feeder),
        Some(Source::Glass) => paper = paper.with_source_in_use(PaperSource::Glass),
        Some(Source::Auto) | None => {}
    }

    let mut view = DeviceView::new(id, scanner.name.clone(), JobKind::Scan)
        .with_connection(scanner.connection.into())
        .with_capabilities(capabilities)
        .with_paper(paper);

    // Readiness indicators only for hardware that exists (or that the status says is there).
    if scanner.feeder || feeder.is_on() {
        view = view.with_indicator(Indicator::new(IndicatorKey::FeederPaper, feeder));
    }
    if scanner.flatbed || flat.is_on() {
        view = view.with_indicator(Indicator::new(IndicatorKey::FlatbedReady, flat));
    }
    if scanner.duplex || duplex.is_on() {
        view = view.with_indicator(Indicator::new(IndicatorKey::DuplexReady, duplex));
    }
    for (key, state) in [
        (IndicatorKey::CoverOpen, flag(|s| s.cover_open)),
        (IndicatorKey::PathCoverOpen, flag(|s| s.path_cover_open)),
        (IndicatorKey::Jam, flag(|s| s.jam)),
        (IndicatorKey::MultiFeed, flag(|s| s.multiple_feed)),
        (IndicatorKey::Attention, flag(|s| s.attention)),
        // The lamp bit is an error flag: there is no readback, so a clear bit is still only "no error reported".
        (IndicatorKey::LampError, flag(|s| s.lamp_error)),
    ] {
        view = view.with_indicator(Indicator::new(key, state));
    }

    let state = if view.problems().any(|i| i.state.is_on()) {
        DeviceState::Attention
    } else if reported.is_some() {
        DeviceState::Online
    } else {
        DeviceState::Unknown
    };
    view.with_state(state)
}
