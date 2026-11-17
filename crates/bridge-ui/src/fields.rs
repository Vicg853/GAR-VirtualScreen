//! Retained text, number and slider controls for views whose rows come and go
//! (the codec accordion swaps its whole content when the codec changes).
use gpui_kit::component::{
    input::{InputEvent, InputState},
    slider::{SliderEvent, SliderState},
};
use gpui_kit::{AppContext, Context, Entity, Focusable, Subscription, Window};
use std::collections::HashMap;

#[derive(Clone, Copy, Default)]
pub struct InputSpec {
    pub placeholder: &'static str,
    pub masked: bool,
    /// Accept digits only, at most this many.
    pub digits: Option<usize>,
}

struct InputField {
    state: Entity<InputState>,
    _subscription: Subscription,
}
struct SliderField {
    state: Entity<SliderState>,
    _subscription: Subscription,
}

/// Controls keyed by what they edit. A key must keep its meaning for as long
/// as the pool lives, because its handler is installed once.
#[derive(Default)]
pub struct Fields {
    inputs: HashMap<String, InputField>,
    sliders: HashMap<String, SliderField>,
}

impl Fields {
    /// Text control showing `value`. `on_event` receives every edit; its flag
    /// is true once the edit is committed with Enter or by leaving the field.
    /// The model value replaces the text whenever the field is not being edited.
    pub fn input<V: 'static>(
        &mut self,
        key: &str,
        value: &str,
        spec: InputSpec,
        window: &mut Window,
        cx: &mut Context<V>,
        on_event: impl Fn(&mut V, String, bool, &mut Window, &mut Context<V>) + 'static,
    ) -> Entity<InputState> {
        if !self.inputs.contains_key(key) {
            let initial = value.to_owned();
            let state = cx.new(|cx| {
                let mut state = InputState::new(window, cx)
                    .placeholder(spec.placeholder)
                    .masked(spec.masked);
                if let Some(digits) = spec.digits {
                    state = state.validate(move |text, _| {
                        text.len() <= digits && text.bytes().all(|b| b.is_ascii_digit())
                    });
                }
                state.set_value(initial, window, cx);
                state
            });
            let subscription = cx.subscribe_in(
                &state,
                window,
                move |view, input, event: &InputEvent, window, cx| {
                    let committed = match event {
                        InputEvent::Change => false,
                        InputEvent::PressEnter { .. } | InputEvent::Blur => true,
                        InputEvent::Focus => return,
                    };
                    let text = input.read(cx).value().to_string();
                    on_event(view, text, committed, window, cx);
                    cx.notify();
                },
            );
            self.inputs.insert(
                key.to_owned(),
                InputField {
                    state,
                    _subscription: subscription,
                },
            );
        }
        let state = self.inputs[key].state.clone();
        let editing = state.read(cx).focus_handle(cx).is_focused(window);
        if !editing && state.read(cx).value().as_ref() != value {
            let value = value.to_owned();
            state.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        state
    }

    /// Slider showing `value`; `on_change` receives each new position.
    #[allow(clippy::too_many_arguments)]
    pub fn slider<V: 'static>(
        &mut self,
        key: &str,
        value: f32,
        min: f32,
        max: f32,
        step: f32,
        window: &mut Window,
        cx: &mut Context<V>,
        on_change: impl Fn(&mut V, f32, &mut Window, &mut Context<V>) + 'static,
    ) -> Entity<SliderState> {
        let value = value.clamp(min, max);
        if !self.sliders.contains_key(key) {
            let state = cx.new(|_| {
                // Maximum first: the state validates each bound against the other.
                SliderState::new()
                    .max(max)
                    .min(min)
                    .step(step)
                    .default_value(value)
            });
            let subscription = cx.subscribe_in(
                &state,
                window,
                move |view, _, event: &SliderEvent, window, cx| {
                    let (SliderEvent::Change(position) | SliderEvent::Release(position)) = event;
                    on_change(view, position.start(), window, cx);
                    cx.notify();
                },
            );
            self.sliders.insert(
                key.to_owned(),
                SliderField {
                    state,
                    _subscription: subscription,
                },
            );
        }
        let state = self.sliders[key].state.clone();
        if (state.read(cx).value().start() - value).abs() >= step {
            state.update(cx, |slider, cx| slider.set_value(value, window, cx));
        }
        state
    }

    /// Current text of a field, if it exists.
    pub fn text(&self, key: &str, cx: &gpui_kit::App) -> Option<String> {
        self.inputs
            .get(key)
            .map(|field| field.state.read(cx).value().to_string())
    }

    /// Replace a field's text, e.g. to clear a password after use.
    pub fn set_text<V: 'static>(
        &self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<V>,
    ) {
        if let Some(field) = self.inputs.get(key) {
            let value = value.to_owned();
            field
                .state
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    /// Drop controls whose rows no longer exist.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.inputs.retain(|key, _| keep(key));
        self.sliders.retain(|key, _| keep(key));
    }
}
