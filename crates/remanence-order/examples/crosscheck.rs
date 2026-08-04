//! Cross-check driver: plans one batch described on stdin and prints
//! the order and total, so the Python reference implementation
//! (`geom-error-sim-v2.py`, private journal) can be compared against
//! the Rust planner on identical wrap maps and target sets.
//!
//! This is a dev-only example binary; the library itself stays pure.
//!
//! Input, line-oriented:
//!
//! ```text
//! geom LTO-9 L9
//! map 61306 122613 ...      # one end_loi per wrap, ascending; last = EOD position
//! start 0                   # or: start -
//! end -                     # or: end <block>
//! objective total           # or: ttf
//! target 5 100              # start_block end_block, repeated
//! ```
//!
//! Output: `order i0 i1 ...` then `total <ns>`.

use remanence_order::{
    lookup_geometry, plan, GeometryLookup, Objective, PlanInput, ReadTarget, ReowpDescriptor,
    WrapMap, PUBLISHED_PRIORS,
};
use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read stdin");

    let mut geom = None;
    let mut descriptors = Vec::new();
    let mut start_block = None;
    let mut end_block = None;
    let mut objective = Objective::MinTotalTime;
    let mut targets = Vec::new();

    for line in input.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("geom") => {
                let generation = parts.next().expect("geom generation");
                let format = parts.next().expect("geom format");
                geom = match lookup_geometry(generation, format) {
                    GeometryLookup::Supported(row) => Some(row),
                    other => panic!("geometry ({generation}, {format}) not supported: {other:?}"),
                };
            }
            Some("map") => {
                for (w, end_loi) in parts.enumerate() {
                    descriptors.push(ReowpDescriptor {
                        partition: 0,
                        wrap_number: w as u32,
                        end_loi: end_loi.parse().expect("end_loi"),
                    });
                }
            }
            Some("start") => {
                let v = parts.next().expect("start value");
                start_block = (v != "-").then(|| v.parse().expect("start block"));
            }
            Some("end") => {
                let v = parts.next().expect("end value");
                end_block = (v != "-").then(|| v.parse().expect("end block"));
            }
            Some("objective") => {
                objective = match parts.next().expect("objective value") {
                    "total" => Objective::MinTotalTime,
                    "ttf" => Objective::MinTimeToFirst,
                    other => panic!("unknown objective {other}"),
                };
            }
            Some("target") => {
                let start = parts.next().expect("target start").parse().expect("start");
                let end = parts.next().expect("target end").parse().expect("end");
                targets.push(ReadTarget {
                    start_block: start,
                    end_block: end,
                });
            }
            Some(other) => panic!("unknown directive {other}"),
            None => {}
        }
    }

    let map = WrapMap::from_descriptors(&descriptors).expect("valid map");
    let result = plan(&PlanInput {
        geometry: geom.expect("geom line required"),
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &targets,
        objective,
        start_block,
        end_block,
    })
    .expect("plan");

    let order: Vec<String> = result
        .hops
        .iter()
        .map(|h| h.target_index.to_string())
        .collect();
    println!("order {}", order.join(" "));
    println!("total {}", result.estimated_total_ns.as_u64());
}
