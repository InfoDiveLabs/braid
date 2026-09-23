//! Spike 3: does cross-application drag & drop actually work?
//!
//!   cargo run -p dl-gui --example dropprobe
//!
//! Slint's docs hedge ("on platforms that support it"), so this has to be tried
//! by hand on each OS. Drag a link from a browser and a file from the file
//! manager onto the window; both the window and stdout report what arrived.
//!
//! A download manager needs two shapes of drop:
//!   * plain text  -> a URL dragged from a browser's address bar or a link
//!   * file paths  -> a .torrent / .txt list dragged from the file manager

slint::slint! {
    export component DropProbe inherits Window {
        title: "Drop probe";
        preferred-width: 620px;
        preferred-height: 380px;
        background: #14161a;

        pure callback describe(data-transfer) -> string;
        in-out property <string> last: "Nothing dropped yet.";
        in-out property <int> count: 0;

        VerticalLayout {
            padding: 20px;
            spacing: 14px;

            Text {
                text: "Cross-application drag & drop probe";
                color: white;
                font-size: 18px;
                font-weight: 700;
            }
            Text {
                text: "Drag a link from your browser, then a file from Finder/Explorer.";
                color: #aab;
                font-size: 12px;
            }

            zone := DropArea {
                can-drop(event) => {
                    // Accept anything, so a rejected drop is distinguishable
                    // from one the platform never delivered at all.
                    DragAction.copy
                }
                dropped(event) => {
                    root.count += 1;
                    root.last = root.describe(event.data);
                    return event.proposed-action;
                }

                Rectangle {
                    border-radius: 14px;
                    border-width: 2px;
                    border-color: zone.has-drag ? #4c8dff : #333a47;
                    background: zone.has-drag ? #4c8dff20 : #1b1e25;
                    animate border-color, background { duration: 120ms; }

                    Text {
                        text: zone.has-drag ? "Release to drop" : "Drop zone";
                        color: zone.has-drag ? #4c8dff : #667;
                        font-size: 14px;
                        font-weight: 600;
                    }
                }
            }

            Text {
                text: "Drops received: " + root.count;
                color: #8f9; font-size: 12px; font-weight: 600;
            }
            Text {
                text: root.last;
                color: white;
                font-size: 12px;
                wrap: word-wrap;
            }
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = DropProbe::new()?;

    ui.on_describe(|data| {
        let mut parts = Vec::new();
        if data.has_plain_text() {
            match data.plain_text() {
                Ok(t) => parts.push(format!("plain text: {t}")),
                Err(e) => parts.push(format!("plain text present but unreadable: {e:?}")),
            }
        }
        if data.has_file_paths() {
            match data.file_paths() {
                Ok(paths) => parts.push(format!(
                    "file paths: {}",
                    paths.map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                )),
                Err(e) => parts.push(format!("file paths present but unreadable: {e:?}")),
            }
        }
        if data.has_image() {
            parts.push("image data".into());
        }
        if parts.is_empty() {
            parts.push("a drop arrived, but it carried no text, files, or image".into());
        }
        let summary = parts.join("\n");
        println!("--- drop ---\n{summary}");
        summary.into()
    });

    println!("Drop probe running. Drag a browser link and a file onto the window.");
    ui.run()
}
