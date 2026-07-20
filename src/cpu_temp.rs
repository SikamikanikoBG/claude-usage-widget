// CPU temperature, read from the Windows "Thermal Zone Information"
// performance counters via PDH (Performance Data Helper).
//
// Why this source and not the obvious one: the commonly cited way to get CPU
// temperature on Windows is the WMI class `MSAcpi_ThermalZoneTemperature` in
// the `root\WMI` namespace -- and that one requires administrator rights.
// Reading it as a normal user fails with "Access to a CIM resource was not
// available to the client", which was confirmed on the development machine.
// This widget is a user-scope tray app that deliberately needs no elevation
// (see the "Start with Windows" HKCU Run entry in `registry.rs`), so asking
// the user to run it as admin just to see a temperature would be a bad
// trade. The other usual suspects -- LibreHardwareMonitor / OpenHardwareMonitor
// WMI namespaces -- are worse still: they need a third-party app installed
// *and* a kernel driver *and* admin.
//
// The `Thermal Zone Information` counter set exposes the same ACPI thermal
// zones with no elevation at all. It's the same data the
// `Win32_PerfFormattedData_Counters_ThermalZoneInformation` WMI class
// surfaces, but reaching it through PDH avoids pulling COM (and a WMI crate,
// and the whole `windows` crate alongside the `windows-sys` already in the
// tree) into a binary that otherwise has none of that.
//
// Two details worth keeping in mind if this ever needs changing:
//
//   * Counter *names* are localized on non-English Windows, so the path
//     "\Thermal Zone Information(*)\..." would not resolve on e.g. a German
//     install. `PdhAddEnglishCounterW` exists precisely for this: it takes
//     the English name regardless of system locale. Using the plain
//     `PdhAddCounterW` here would be a latent bug that only shows up on other
//     people's machines.
//   * Values are in KELVIN, not Celsius. The high-precision counter reports
//     tenths of a Kelvin.

use std::ffi::c_void;

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA,
};

/// Wildcard counter path covering every thermal zone the firmware exposes.
/// Preferred because it reports tenths of a Kelvin rather than whole Kelvin,
/// which is the difference between the tray reading "61" vs "62" as the
/// machine drifts across a degree boundary.
const HIGH_PRECISION_PATH: &str = r"\Thermal Zone Information(*)\High Precision Temperature";

/// Fallback for firmware/builds that expose only the whole-Kelvin counter.
const WHOLE_KELVIN_PATH: &str = r"\Thermal Zone Information(*)\Temperature";

/// Absolute zero offset, for the Kelvin -> Celsius conversion.
const KELVIN_OFFSET: f64 = 273.15;

/// Plausibility window for a thermal-zone reading, in Celsius. Zones that are
/// present in the counter set but not actually wired to a sensor report 0
/// Kelvin (i.e. -273 C) -- the development machine's `\_TZ.CHGZ` (charger)
/// zone does exactly this. Without a sanity filter, "pick the hottest zone"
/// would be fine but "pick the only zone" or an average would silently
/// produce nonsense. The upper bound catches equally-broken readings in the
/// other direction; a CPU at 150 C has bigger problems than a tray widget.
const MIN_PLAUSIBLE_C: f64 = 5.0;
const MAX_PLAUSIBLE_C: f64 = 125.0;

/// Which counter a [`CpuTempReader`] ended up bound to, so the conversion
/// knows whether it's holding tenths of a Kelvin or whole Kelvin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scale {
    /// `High Precision Temperature`: tenths of a Kelvin.
    DeciKelvin,
    /// `Temperature`: whole Kelvin.
    Kelvin,
}

impl Scale {
    fn to_celsius(self, raw: f64) -> f64 {
        match self {
            Scale::DeciKelvin => raw / 10.0 - KELVIN_OFFSET,
            Scale::Kelvin => raw - KELVIN_OFFSET,
        }
    }
}

/// An open PDH query against the thermal-zone counters. Constructed once and
/// reused for every sample -- opening a query per read would work, but this
/// runs on a timer for the lifetime of the process, so the handles are worth
/// holding onto.
pub struct CpuTempReader {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    scale: Scale,
}

// The PDH handles are just opaque pointers into pdh.dll's own state; the
// reader is created on, and confined to, the temperature worker thread, and
// is never shared between threads. `Send` is asserted only so the reader can
// be constructed and then moved into that thread.
unsafe impl Send for CpuTempReader {}

impl CpuTempReader {
    /// Opens the query and binds the best available thermal-zone counter, or
    /// returns `None` if this machine exposes no usable thermal zone at all
    /// (which is a legitimate outcome -- plenty of desktops and VMs don't).
    pub fn new() -> Option<Self> {
        let mut query: PDH_HQUERY = std::ptr::null_mut();
        // SAFETY: `query` is a valid out-pointer; PDH fills it on success.
        let status = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if status != 0 {
            eprintln!("[claude-usage-widget] cpu temp: PdhOpenQueryW failed (0x{status:08X})");
            return None;
        }

        // Prefer the tenths-of-a-Kelvin counter; fall back to whole Kelvin.
        let bound = add_counter(query, HIGH_PRECISION_PATH)
            .map(|counter| (counter, Scale::DeciKelvin))
            .or_else(|| {
                add_counter(query, WHOLE_KELVIN_PATH).map(|counter| (counter, Scale::Kelvin))
            });

        let (counter, scale) = match bound {
            Some(bound) => bound,
            None => {
                eprintln!(
                    "[claude-usage-widget] cpu temp: no thermal zone counter on this machine; \
                     temperature will be unavailable"
                );
                // SAFETY: `query` was successfully opened above and is not
                // used again after this point.
                unsafe { PdhCloseQuery(query) };
                return None;
            }
        };

        let reader = CpuTempReader { query, counter, scale };

        // Prime the query. PDH wants at least one collection before formatted
        // values are available; doing it here means the first real read after
        // construction returns data rather than an empty first sample.
        // SAFETY: handles are valid and owned by `reader`.
        unsafe { PdhCollectQueryData(reader.query) };

        eprintln!("[claude-usage-widget] cpu temp: bound thermal zone counter ({scale:?})");
        Some(reader)
    }

    /// Takes one sample and reduces every thermal zone to a single CPU
    /// temperature in whole degrees Celsius, or `None` if nothing plausible
    /// came back this time.
    pub fn read_celsius(&self) -> Option<f64> {
        // SAFETY: `self.query` is a live query owned by `self`.
        let status = unsafe { PdhCollectQueryData(self.query) };
        if status != 0 {
            return None;
        }

        let zones = self.sample_zones()?;
        pick_cpu_zone(&zones)
    }

    /// Reads the wildcard counter's per-instance values as
    /// `(zone name, celsius)` pairs.
    fn sample_zones(&self) -> Option<Vec<(String, f64)>> {
        // Standard two-call PDH pattern: ask with a zero-sized buffer to
        // learn the required size, then call again with a buffer that big.
        let mut buffer_size: u32 = 0;
        let mut item_count: u32 = 0;
        // SAFETY: passing a null item buffer with size 0 is the documented
        // way to query the required buffer size; PDH writes only the two
        // out-params in this mode.
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                std::ptr::null_mut(),
            )
        };
        if status != PDH_MORE_DATA || buffer_size == 0 {
            return None;
        }

        // PDH returns a single block containing an array of
        // PDH_FMT_COUNTERVALUE_ITEM_W structs followed by the instance-name
        // strings those structs point into. The buffer therefore has to be
        // allocated as bytes (it's larger than the struct array alone) but
        // aligned for the struct type -- hence a Vec of the struct type sized
        // up to cover `buffer_size` bytes, rather than a Vec<u8>.
        let struct_size = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let capacity = (buffer_size as usize).div_ceil(struct_size).max(1);
        let mut buffer: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![Default::default(); capacity];

        // SAFETY: `buffer` is at least `buffer_size` bytes and correctly
        // aligned for the item struct.
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut buffer_size,
                &mut item_count,
                buffer.as_mut_ptr(),
            )
        };
        if status != 0 {
            return None;
        }

        let mut zones = Vec::with_capacity(item_count as usize);
        for item in buffer.iter().take(item_count as usize) {
            // A per-instance error (CStatus != 0) means this one zone's value
            // is not valid this sample; skip it rather than discarding the
            // whole reading.
            if item.FmtValue.CStatus != 0 {
                continue;
            }
            // SAFETY: on success PDH guarantees `szName` points at a
            // NUL-terminated wide string inside `buffer`, which is still
            // alive here.
            let name = unsafe { wide_to_string(item.szName) };
            // SAFETY: the value was requested as PDH_FMT_DOUBLE, so the
            // `doubleValue` union member is the populated one.
            let raw = unsafe { item.FmtValue.Anonymous.doubleValue };
            zones.push((name, self.scale.to_celsius(raw)));
        }

        Some(zones)
    }
}

impl Drop for CpuTempReader {
    fn drop(&mut self) {
        // SAFETY: `query` was opened in `new` and closing it also releases
        // the counter handle added to it.
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// Adds one English-named counter path to `query`, returning its handle.
fn add_counter(query: PDH_HQUERY, path: &str) -> Option<PDH_HCOUNTER> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `counter` is a
    // valid out-pointer.
    let status = unsafe { PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) };
    if status != 0 {
        return None;
    }
    Some(counter)
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `ptr` must be null or point at a NUL-terminated wide string.
unsafe fn wide_to_string(ptr: *mut u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const c_void as *const u16, len) };
    String::from_utf16_lossy(slice)
}

/// Reduces every thermal zone to the one number worth showing.
///
/// Zone naming is firmware-specific, so this cannot just hardcode a name.
/// The development machine (a Ryzen laptop) exposes `\_TZ.CPUZ`, `\_TZ.GFXZ`,
/// `\_TZ.BATZ`, `\_TZ.LOCZ`, `\_TZ.EXTZ` and `\_TZ.CHGZ`, but other firmware
/// commonly exposes a single generic `\_TZ.TZ00`, and some expose nothing
/// CPU-specific at all. So:
///
///   1. Drop implausible readings (unwired zones report 0 Kelvin).
///   2. If a zone names itself as the CPU one, trust it.
///   3. Otherwise fall back to the hottest remaining zone, which on a machine
///      with only generic zones is the closest available proxy for "is this
///      thing running hot", and is never misleadingly *low*.
fn pick_cpu_zone(zones: &[(String, f64)]) -> Option<f64> {
    let plausible: Vec<&(String, f64)> = zones
        .iter()
        .filter(|(_, c)| (MIN_PLAUSIBLE_C..=MAX_PLAUSIBLE_C).contains(c))
        .collect();

    if plausible.is_empty() {
        return None;
    }

    if let Some((_, c)) = plausible
        .iter()
        .find(|(name, _)| name.to_ascii_uppercase().contains("CPU"))
    {
        return Some(*c);
    }

    plausible
        .iter()
        .map(|(_, c)| *c)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    #[test]
    fn deci_kelvin_converts_to_celsius() {
        // 3342 tenths of a Kelvin is the literal reading observed on the
        // development machine while building this.
        let c = Scale::DeciKelvin.to_celsius(3342.0);
        assert!((c - 61.05).abs() < 0.001, "got {c}");
    }

    #[test]
    fn whole_kelvin_converts_to_celsius() {
        let c = Scale::Kelvin.to_celsius(335.0);
        assert!((c - 61.85).abs() < 0.001, "got {c}");
    }

    #[test]
    fn a_cpu_named_zone_wins_even_when_another_zone_is_hotter() {
        // The GPU running hotter than the CPU must not hijack a reading
        // that's explicitly labelled as the CPU's.
        let picked = pick_cpu_zone(&zones(&[("\\_TZ.GFXZ", 91.0), ("\\_TZ.CPUZ", 61.0)]));
        assert_eq!(picked, Some(61.0));
    }

    #[test]
    fn unwired_zones_reporting_absolute_zero_are_ignored() {
        // `\_TZ.CHGZ` reports 0 Kelvin (-273.15 C) on the development
        // machine. Picking it, or averaging it in, would be nonsense.
        let picked = pick_cpu_zone(&zones(&[("\\_TZ.CHGZ", -273.15), ("\\_TZ.LOCZ", 44.0)]));
        assert_eq!(picked, Some(44.0));
    }

    #[test]
    fn falls_back_to_the_hottest_zone_when_none_is_cpu_named() {
        // Firmware exposing only generic zones: the hottest is the safest
        // proxy, since it can't under-report a machine that's cooking.
        let picked = pick_cpu_zone(&zones(&[("\\_TZ.TZ00", 48.0), ("\\_TZ.TZ01", 55.0)]));
        assert_eq!(picked, Some(55.0));
    }

    #[test]
    fn no_plausible_zone_yields_nothing_rather_than_a_wrong_number() {
        assert_eq!(pick_cpu_zone(&zones(&[("\\_TZ.CHGZ", -273.15)])), None);
        assert_eq!(pick_cpu_zone(&[]), None);
        // Absurdly high readings are treated as broken too.
        assert_eq!(pick_cpu_zone(&zones(&[("\\_TZ.CPUZ", 400.0)])), None);
    }

    /// Exercises the real PDH plumbing end to end against whatever hardware
    /// this is running on. Everything else in this module tests pure
    /// conversion/selection logic, which would happily keep passing even if
    /// the counter binding or the buffer-sizing dance were completely broken.
    ///
    /// Deliberately does NOT fail when no sensor is present: plenty of
    /// machines (notably the CI runners, which are VMs) expose no thermal
    /// zone at all, and "this hardware has no sensor" is a supported outcome
    /// the app handles, not a regression. What it does assert is that *if* a
    /// reading comes back, it's a physically plausible one -- which is what
    /// would catch a botched Kelvin conversion or a misread union member.
    #[test]
    fn live_hardware_reading_is_plausible_when_a_sensor_exists() {
        match CpuTempReader::new() {
            None => println!("no thermal zone counter on this machine; nothing to check"),
            Some(reader) => match reader.read_celsius() {
                None => println!("thermal zone counter present but returned no usable sample"),
                Some(c) => {
                    println!("live CPU temperature reading: {c:.2} C");
                    assert!(
                        (MIN_PLAUSIBLE_C..=MAX_PLAUSIBLE_C).contains(&c),
                        "live reading {c} C is outside the plausible range -- \
                         suspect the Kelvin conversion or the counter binding"
                    );
                }
            },
        }
    }

    #[test]
    fn a_cpu_zone_is_still_skipped_when_its_own_reading_is_implausible() {
        // Plausibility filtering has to happen before the CPU-name
        // preference, or a broken CPU zone would be preferred over a working
        // generic one.
        let picked = pick_cpu_zone(&zones(&[("\\_TZ.CPUZ", -273.15), ("\\_TZ.TZ00", 52.0)]));
        assert_eq!(picked, Some(52.0));
    }
}
