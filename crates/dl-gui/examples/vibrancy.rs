//! Manual check for macOS window vibrancy.
//!
//!   cargo run -p dl-gui --features vibrancy --example vibrancy
//!
//! Cannot be verified headlessly: blur is applied by the compositor to whatever
//! is *behind* the window, so it does not exist in an offscreen render. Someone
//! has to look. Findings are in docs/spikes.md.

use dl_gui::platform::{self, Vibrancy};
use slint::ComponentHandle as _;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let shell = platform::install(Vibrancy::Requested);
    println!(
        "vibrancy granted: {}\n{}",
        shell.vibrancy,
        if shell.vibrancy {
            "The desktop behind this window should look blurred."
        } else {
            "Not available on this platform or build; the window will be opaque."
        }
    );

    let ui = VibrancyProbe::new()?;
    // Paint a translucent background only if the platform actually granted
    // blur. Requesting and not receiving would otherwise leave a plain
    // see-through window, which reads as a bug rather than a style.
    ui.set_vibrant(shell.vibrancy);
    ui.run()?;
    Ok(())
}

slint::slint! {
    export component VibrancyProbe inherits Window {
        title: "Vibrancy probe";
        preferred-width: 560px;
        preferred-height: 360px;

        in property <bool> vibrant;
        background: root.vibrant ? #1c1c1eb0 : #1c1c1e;

        VerticalLayout {
            padding: 32px;
            spacing: 14px;
            alignment: start;

            Text {
                text: root.vibrant ? "Vibrancy granted" : "Vibrancy unavailable: opaque";
                font-size: 26px;
                color: white;
            }
            Text {
                text: root.vibrant
                    ? "The desktop behind this window should be blurred."
                    : "This window is deliberately opaque.";
                color: #ffffffcc;
            }
            Rectangle {
                height: 84px;
                background: #ffffff22;
                border-radius: 14px;
                border-width: 1px;
                border-color: #ffffff33;
                Text {
                    text: "A card on the backdrop";
                    color: white;
                }
            }
        }
    }
}
