//! (El1xr) What the finders found, kept in the settings file as `VICTORY_MOD_MENU_FOUND` so later
//! launches use it without searching: `tp=<hash>:<slots>;health=<hash>:<slots>:<float>;
//! armour=<hash>:<slots>:<float>:<getter>;wanted=<hash>:<slots>:<float>:<getter>;
//! weather=<hash>;vehicle=<create>:<slots>:<model>:<request>:<loaded>` (hashes in hex, float 0 or
//! 1, absent parts not found). Read at start ([`apply`]); written by Find all ([`save`]).

use super::{stats, teleport, vehicle, weather};

pub const SETTING: &str = "VICTORY_MOD_MENU_FOUND";

fn hex(v: &str) -> Option<u64> {
    u64::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
}

fn flag(v: &str) -> Option<bool> {
    match v.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn slots(v: &str) -> Option<usize> {
    v.trim()
        .parse()
        .ok()
        .filter(|n| (1..=super::natives::MAX_ARGS).contains(n))
}

/// This run's finds as the setting's value.
pub fn format() -> String {
    let mut parts = Vec::new();
    if let Some((h, n)) = teleport::native() {
        parts.push(format!("tp={h:x}:{n}"));
    }
    if let Some((h, n, f)) = stats::health_found() {
        parts.push(format!("health={h:x}:{n}:{}", u8::from(f)));
    }
    if let Some((h, n, f, g)) = stats::armour_found() {
        parts.push(format!("armour={h:x}:{n}:{}:{g:x}", u8::from(f)));
    }
    if let Some((h, n, f, g)) = stats::wanted_found() {
        parts.push(format!("wanted={h:x}:{n}:{}:{g:x}", u8::from(f)));
    }
    if let Some((h, _)) = weather::found() {
        parts.push(format!("weather={h:x}"));
    }
    if let Some(v) = vehicle::found() {
        parts.push(format!(
            "vehicle={:x}:{}:{:x}:{:x}:{:x}",
            v.create, v.slots, v.model, v.request, v.loaded
        ));
    }
    parts.join(";")
}

/// Takes the finds in `v`; returns a line per part (taken, or why not).
pub fn apply(v: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for part in v.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((key, rest)) = part.split_once('=') else {
            lines.push(format!("found {part}: ignored (no =)"));
            continue;
        };
        let f: Vec<&str> = rest.split(':').collect();
        let ok = match (key.trim(), f.as_slice()) {
            ("tp", [h, n]) => hex(h).zip(slots(n)).filter(|(_, n)| *n >= 4).map(|(h, n)| {
                teleport::set_native(h, n);
            }),
            ("health", [h, n, fl]) => (|| {
                stats::set_health_found(hex(h)?, slots(n)?, flag(fl)?);
                Some(())
            })(),
            ("armour", [h, n, fl, g]) => (|| {
                stats::set_armour_found(hex(h)?, slots(n)?, flag(fl)?, hex(g)?);
                Some(())
            })(),
            ("wanted", [h, n, fl, g]) => (|| {
                stats::set_wanted_found(hex(h)?, slots(n)?, flag(fl)?, hex(g)?);
                Some(())
            })(),
            ("weather", [h]) => hex(h).filter(|&h| weather::set_found(h)).map(|_| ()),
            ("vehicle", [c, n, m, r, l]) => (|| {
                vehicle::set_found(vehicle::Found {
                    create: hex(c)?,
                    slots: slots(n)?,
                    model: u32::try_from(hex(m)?).ok()?,
                    request: hex(r)?,
                    loaded: hex(l)?,
                });
                Some(())
            })(),
            _ => None,
        };
        lines.push(match ok {
            Some(()) => format!("found {part}: taken"),
            None => format!("found {part}: ignored (bad value)"),
        });
    }
    lines
}

/// Writes `line` to the settings file.
pub fn save(line: &str) -> Result<(), String> {
    victory_core::settings::set(SETTING, line).map_err(|e| format!("{e:?}"))?;
    victory_core::settings::save().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_round_trip_through_the_setting() {
        let v = "tp=4a6560c618a11572:7;health=51:4:0;armour=52:3:1:62;wanted=99f1dcca3222b48b:3:0:b649eb465f68c63d;\
                 weather=5e2192a205c1c357;vehicle=c22a05a7045f73af:12:39da2754:b07552fe99d2dbbd:915253d05ec4541";
        let lines = apply(v);
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|l| l.ends_with(": taken")), "{lines:?}");
        // Other tests set the teleport native and the stats setters too (the same values for the
        // stats), so only the parts no other test touches are compared whole.
        let out = format();
        for part in [
            "armour=52:3:1:62",
            "wanted=99f1dcca3222b48b:3:0:b649eb465f68c63d",
            "weather=5e2192a205c1c357",
            "vehicle=c22a05a7045f73af:12:39da2754:b07552fe99d2dbbd:915253d05ec4541",
        ] {
            assert!(out.split(';').any(|p| p == part), "{part} in {out}");
        }
        assert_eq!(vehicle::found().unwrap().model, 0x39da_2754);
    }

    #[test]
    fn bad_parts_are_ignored_one_by_one() {
        let lines = apply("tp=zz:7; weather=1234 ;health=51:4:2;nonsense;tp=4a6560c618a11572:2");
        assert_eq!(
            lines,
            vec![
                "found tp=zz:7: ignored (bad value)",
                "found weather=1234: ignored (bad value)",
                "found health=51:4:2: ignored (bad value)",
                "found nonsense: ignored (no =)",
                "found tp=4a6560c618a11572:2: ignored (bad value)",
            ]
        );
    }
}
