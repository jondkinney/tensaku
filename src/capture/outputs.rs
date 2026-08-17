//! Named `wl_output` discovery shared by screencopy and the virtual pointer.
//!
//! Every scroll-capture Wayland client used to bind whichever `wl_output`
//! global happened to be advertised first, which is only correct on a
//! single-monitor desktop. The overlay knows which monitor it covers (its
//! connector name, e.g. `DP-3`); this tracker binds every output, collects
//! their `name` and current `mode`, and hands back the matching proxy so
//! captures and pointer warps target the same screen the user selected on.

use anyhow::{Context, Result};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output;
use wayland_client::{Dispatch, Proxy, QueueHandle, WEnum};

#[derive(Debug, Default, Clone)]
pub struct OutputInfo {
    /// Connector name from `wl_output.name` (v4+); `None` on older compositors.
    pub name: Option<String>,
    /// Current mode in physical pixels.
    pub mode: Option<(i32, i32)>,
}

#[derive(Default)]
pub struct OutputTracker {
    outputs: Vec<(wl_output::WlOutput, OutputInfo)>,
}

impl OutputTracker {
    /// Bind every advertised `wl_output` (up to v4 so `name` is delivered).
    /// Call before the roundtrip that should deliver the output events.
    pub fn bind_all<D>(&mut self, globals: &GlobalList, qh: &QueueHandle<D>)
    where
        D: Dispatch<wl_output::WlOutput, ()> + 'static,
    {
        let registry = globals.registry();
        globals.contents().with_list(|list| {
            for global in list {
                if global.interface != wl_output::WlOutput::interface().name {
                    continue;
                }
                let output = registry.bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    global.version.min(4),
                    qh,
                    (),
                );
                self.outputs.push((output, OutputInfo::default()));
            }
        });
    }

    /// Record a `wl_output` event; call from the client's `Dispatch` impl.
    pub fn handle(&mut self, output: &wl_output::WlOutput, event: wl_output::Event) {
        let Some((_, info)) = self.outputs.iter_mut().find(|(o, _)| o == output) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => info.mode = Some((width, height)),
            _ => {}
        }
    }

    /// The output whose connector is `target`, or the first advertised output
    /// when no target is known or nothing matches (single-monitor behavior).
    pub fn select(&self, target: Option<&str>) -> Result<(&wl_output::WlOutput, &OutputInfo)> {
        let (first, first_info) = self.outputs.first().context("no wl_output globals")?;
        let Some(target) = target else {
            return Ok((first, first_info));
        };
        match self
            .outputs
            .iter()
            .find(|(_, info)| info.name.as_deref() == Some(target))
        {
            Some((output, info)) => Ok((output, info)),
            None => {
                if self.outputs.len() > 1 {
                    eprintln!(
                        "capture: no wl_output named {target}; using the first of {} outputs",
                        self.outputs.len()
                    );
                }
                Ok((first, first_info))
            }
        }
    }
}
