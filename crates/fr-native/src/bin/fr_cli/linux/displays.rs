//! One authenticated, approved metadata lookup. No renderer, worker or reconnect
//! is needed; output is written only after the original local session is closed.
use super::{
    Cell, Client, Configuration, Cx, Duration, Failure, LocalApi, ObserverPolicy, PeerSelector,
    Runtime, Shutdown, Target, failure, offer, output, read_roots, tailnet,
};
use fr_wire::display::Catalog;
use frd::{native_connection, session_startup::ObserverError};
use std::fmt::Write;

pub(super) fn run(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    target: &Target,
    json: bool,
) -> Result<String, Failure> {
    let mut client =
        Client::new(api, read_roots(&target.roots)?, Duration::from_secs(5)).map_err(|_| {
            failure(
                "invalid_native_configuration",
                "Verify the local CA roots and native connection policy.",
            )
        })?;
    let selector = if target.by_name {
        PeerSelector::Name(&target.node)
    } else {
        PeerSelector::StableId(&target.node)
    };
    let cfg = Configuration {
        port: target.port,
        family: if target.ipv6 {
            super::AddressFamily::Ipv6
        } else {
            super::AddressFamily::Ipv4
        },
        ..Default::default()
    };
    let operation = client.run(cx.clone(), selector, cfg, offer(), |viewer| {
        viewer.inspect_displays(ObserverPolicy::default(), |_| Ok(()))
    });
    let result = runtime.block_on(shutdown.run(cx, stopped, operation));
    if stopped.get() {
        return Err(Failure::new(
            "cancelled",
            "Display inspection stopped; no display was selected and no native worker was started.",
            130,
        ));
    }
    let catalog = result
        .map_err(connection_error)?
        .map_err(inspection_error)?;
    Ok(render(&catalog, json))
}
fn connection_error(error: native_connection::Error) -> Failure {
    if let Some(refused) = super::refusal::connection(error) {
        return refused;
    }
    match error {
        native_connection::Error::Tailnet(error) => tailnet(error),
        _ => failure(
            "display_connection_failed",
            "Verify host identity, native transport and host availability; inspection does not bypass admission.",
        ),
    }
}
fn inspection_error(error: ObserverError) -> Failure {
    if let Some(refused) = super::refusal::observation(error) {
        return refused;
    }
    match error {
        ObserverError::Expired => failure(
            "display_inspection_timeout",
            "Check local approval on the host and its display catalog; no screen was selected.",
        ),
        _ => failure(
            "display_inspection_refused",
            "Check local approval, display capability and session validity on the host; no screen was selected.",
        ),
    }
}
fn render(catalog: &Catalog, json: bool) -> String {
    let mut rows = String::new();
    for (index, display) in catalog.displays().iter().enumerate() {
        if json {
            if index != 0 {
                rows.push(',');
            }
            // Wire handles are u128 and generations/revisions are u64. Keep them
            // decimal strings so JSON consumers cannot round them through f64.
            let _ = write!(
                rows,
                "{{\"handle\":\"{}\",\"geometry_generation\":\"{}\",\"x\":{},\"y\":{},\"pixel_width\":{},\"pixel_height\":{},\"logical_width\":{},\"logical_height\":{},\"scale_numerator\":{},\"scale_denominator\":{},\"rotation_quarter_turns\":{}}}",
                display.handle,
                display.geometry.as_raw(),
                display.x,
                display.y,
                display.pixel_width,
                display.pixel_height,
                display.logical_width,
                display.logical_height,
                display.scale_numerator,
                display.scale_denominator,
                display.rotation
            );
        } else {
            let _ = writeln!(
                rows,
                "{}  {}x{} px at ({}, {})  logical {}x{}  scale {}/{}  rotation {} quarter turn(s)  geometry {}",
                display.handle,
                display.pixel_width,
                display.pixel_height,
                display.x,
                display.y,
                display.logical_width,
                display.logical_height,
                display.scale_numerator,
                display.scale_denominator,
                display.rotation,
                display.geometry.as_raw()
            );
        }
    }
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"success\",\"snapshot_only\":true,\"session_closed\":true,\"remote_cleanup_confirmed\":false,\"display_selected\":false,\"input_requested\":false,\"decoder_started\":false,\"transport_qualified\":false,\"catalog_revision\":\"{}\",\"displays\":[{}]}}\n",
            output::timestamp(),
            catalog.revision(),
            rows
        )
    } else {
        format!(
            "Approved host display catalog (revision {}, {} display(s)):\n{}Snapshot only: this local inspection session is closed; remote cleanup is unconfirmed. Handles are session-local; a new connection revalidates its current catalog. No screen, decoder or input was started.\n",
            catalog.revision(),
            catalog.displays().len(),
            rows
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
    use fr_wire::display::{Display, MAX_DISPLAYS};
    fn check_json(output: &str, assertions: &str) {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let script = format!(
            "import sys,json\nx=json.load(sys.stdin)\nassert x['schema_version']==1\n{assertions}"
        );
        let mut parser = Command::new("python3")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        parser
            .stdin
            .take()
            .unwrap()
            .write_all(output.as_bytes())
            .unwrap();
        assert!(parser.wait().unwrap().success());
    }
    #[test]
    fn inventory_json_is_lossless_bounded_and_explicit_about_closed_snapshot_scope() {
        let entries = (0..MAX_DISPLAYS)
            .map(|i| Display {
                handle: u128::MAX - u128::try_from(i).unwrap(),
                geometry: DisplayGeometryGeneration::from_raw(u64::MAX),
                x: -1920,
                y: 24,
                pixel_width: 1920,
                pixel_height: 1080,
                logical_width: 1280,
                logical_height: 720,
                scale_numerator: 3,
                scale_denominator: 2,
                rotation: 1,
            })
            .collect::<Vec<_>>();
        let catalog = Catalog::new(u64::MAX, &entries, &ProtocolLimits::ABSOLUTE).unwrap();
        let output = render(&catalog, true);
        check_json(
            &output,
            "assert x['catalog_revision']==str(2**64-1)\nassert len(x['displays'])==8\nr=x['displays'][0]\nassert r['handle']==str(2**128-1)\nassert r['geometry_generation']==str(2**64-1)\nassert r['x']==-1920 and r['scale_numerator']==3 and r['rotation_quarter_turns']==1\nassert x['snapshot_only'] and x['session_closed']\nassert not any(x[k] for k in ['input_requested','decoder_started','display_selected','transport_qualified','remote_cleanup_confirmed'])",
        );
        assert!(output.len() < 8192);
        let human = render(&catalog, false);
        assert!(
            human.contains(&u128::MAX.to_string()) && human.contains("1920x1080 px at (-1920, 24)")
        );
        assert!(human.contains("remote cleanup is unconfirmed"));
    }
    #[test]
    fn empty_inventory_is_successful_metadata_not_invented_display_availability() {
        let catalog = Catalog::new(7, &[], &ProtocolLimits::ABSOLUTE).unwrap();
        check_json(
            &render(&catalog, true),
            "assert x['displays']==[]\nassert x['outcome']=='success'\nassert not x['remote_cleanup_confirmed']",
        );
        assert!(render(&catalog, false).contains("0 display(s)"));
    }
}
