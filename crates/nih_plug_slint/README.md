# nih_plug_slint

`nih_plug_slint` is an adapter for embedding [Slint](https://slint.dev/) UIs in
NIH-plug editors. It uses `baseview` for host window embedding and Slint's
FemtoVG renderer for OpenGL drawing.

Add the adapter next to `nih_plug` in your plugin crate:

```toml
[dependencies]
nih_plug = { git = "https://github.com/mkw2000/nih-plug.git", features = ["assert_process_allocs"] }
nih_plug_slint = { git = "https://github.com/mkw2000/nih-plug.git" }
```

Store a `SlintEditorState` in your parameters so NIH-plug can persist the editor
size:

```rust
use nih_plug::prelude::*;
use nih_plug_slint::{SlintEditor, SlintEditorState};
use std::sync::Arc;

#[derive(Params)]
struct MyParams {
    #[persist = "editor-state"]
    editor_state: Arc<SlintEditorState>,
}
```

Return a `SlintEditor` from your plugin's `editor()` method. The factory closure
should create your generated Slint component, and `with_event_loop()` can be used
to push parameter values into the UI and wire callbacks back to NIH-plug.

```rust
fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
    let params = self.params.clone();

    Some(Box::new(
        SlintEditor::new(params.editor_state.clone(), || gui::AppWindow::new())
            .with_event_loop(move |handler, _setter, _window| {
                handler.component().set_gain(params.gain.value());
            }),
    ))
}
```

The adapter re-exports `slint`, so consumers can use `nih_plug_slint::slint`
when they need access to Slint types from the same dependency graph.
