//! Input backend for emulated input events received via [EI](https://libinput.pages.freedesktop.org/libei/).

use reis::Interface;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisRelativeDirection, AxisSource, ButtonState, Device,
    DeviceCapability, Event, InputBackend, InputTime, KeyState, KeyboardKeyEvent, Keycode,
    PointerAxisEvent, PointerButtonEvent, PointerMotionAbsoluteEvent, PointerMotionEvent,
    TouchCancelEvent, TouchDownEvent, TouchEvent, TouchFrameEvent, TouchMotionEvent, TouchSlot,
    TouchUpEvent, UnusedEvent,
};
use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};

use crate::input::backend_ext::NiriInputDevice;
use crate::niri::State;
use crate::utils::RemoteDesktopSessionId;

pub struct EisInputBackend;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct EisVirtualDevice {
    /// Remote desktop session ID
    pub session_id: RemoteDesktopSessionId,
    /// Device ID unique to the remote desktop session
    pub device_id: u64,
    /// Keyboard source for origin-aware input tracking. Set to
    /// [`smithay::input::keyboard::KeyboardSource::MAIN`] for non-keyboard devices or when
    /// source tracking is not needed.
    pub source: smithay::input::keyboard::KeyboardSource,
}

impl InputBackend for EisInputBackend {
    type Device = EisVirtualDevice;

    type KeyboardKeyEvent = EisEventAdapter<reis::request::KeyboardKey, PressedCount>;

    type PointerAxisEvent = EisEventAdapter<reis::request::Frame, ScrollFrame>;
    type PointerButtonEvent = EisEventAdapter<reis::request::Button>;
    type PointerMotionEvent = EisEventAdapter<reis::request::PointerMotion>;
    type PointerMotionAbsoluteEvent =
        EisEventAdapter<reis::request::PointerMotionAbsolute, AbsolutePositionEventExtra>;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = EisEventAdapter<reis::request::TouchDown, AbsolutePositionEventExtra>;
    type TouchMotionEvent = EisEventAdapter<reis::request::TouchMotion, AbsolutePositionEventExtra>;
    type TouchUpEvent = EisEventAdapter<reis::request::TouchUp>;
    type TouchCancelEvent = EisEventAdapter<reis::request::TouchCancel>;
    type TouchFrameEvent = EisEventAdapter<reis::request::Frame, TouchFrame>;

    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}

impl Device for EisVirtualDevice {
    fn id(&self) -> String {
        format!(
            "Remote desktop (EIS) virtual device {}/{}",
            self.session_id, self.device_id
        )
    }

    fn name(&self) -> String {
        String::from("Remote desktop (EIS) virtual device")
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        // TODO: only actual EIS selected capabilities?
        matches!(
            capability,
            DeviceCapability::Keyboard | DeviceCapability::Pointer | DeviceCapability::Touch
        )
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<std::path::PathBuf> {
        None
    }
}

impl NiriInputDevice for EisVirtualDevice {
    fn output(&self, _state: &State) -> Option<Output> {
        // This would map local output coordinates to global output
        // coordinates if devices were per-output.
        None
    }

    fn source(&self) -> smithay::input::keyboard::KeyboardSource {
        self.source
    }
}

/// Wrapper to implement [`Event`] automatically and to hold extra data
pub struct EisEventAdapter<Ev, Extra = ()> {
    /// Remote desktop session ID
    pub session_id: RemoteDesktopSessionId,
    /// Keyboard source identifying this remote session's virtual device origin.
    pub source: smithay::input::keyboard::KeyboardSource,
    pub inner: Ev,
    pub extra: Extra,
}

impl<Ev: reis::request::EventTime, Extra> Event<EisInputBackend> for EisEventAdapter<Ev, Extra> {
    fn time(&self) -> InputTime {
        InputTime::from_micros(self.inner.time())
    }

    fn device(&self) -> <EisInputBackend as InputBackend>::Device {
        EisVirtualDevice {
            session_id: self.session_id,
            device_id: self.inner.device().device().as_object().id(),
            source: self.source,
        }
    }
}

//-----------------//
// KEYBOARD EVENTS //
//-----------------//

/// Extra passed to the keyboard key event containing the number of keys pressed on all devices in
/// the seat.
pub struct PressedCount(pub u32);

impl KeyboardKeyEvent<EisInputBackend>
    for EisEventAdapter<reis::request::KeyboardKey, PressedCount>
{
    fn key_code(&self) -> Keycode {
        // Offset from evdev keycodes (where KEY_ESCAPE is 1) to X11 keycodes
        Keycode::new(self.inner.key + 8)
    }

    fn state(&self) -> KeyState {
        match self.inner.state {
            reis::ei::keyboard::KeyState::Released => KeyState::Released,
            reis::ei::keyboard::KeyState::Press => KeyState::Pressed,
        }
    }

    fn count(&self) -> u32 {
        // Smithay does this already...
        self.extra.0
    }
}

//----------------//
// POINTER EVENTS //
//----------------//

#[derive(Default)]
pub struct ScrollFrame {
    /// Continuous scrolling, like on a touchpad.
    pub delta: Option<(f32, f32)>,
    /// 120-notch scrolling, like on a traditional mouse wheel.
    ///
    /// According to the EI protocol, 120 in discrete is the same as 1.0 in delta.
    pub discrete: Option<(i32, i32)>,
    /// Last bool denotes `is_cancel`
    pub stop: Option<((bool, bool), bool)>,
}

impl ScrollFrame {
    fn amount(&self, axis: Axis) -> Option<f64> {
        if self.stop.is_some_and(|(axes, _)| tuple_axis(axes, axis)) {
            return Some(0.0);
        }

        self.delta.and_then(|delta| {
            let amount = tuple_axis(delta, axis) as f64;
            (amount != 0.0).then_some(amount)
        })
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        // EI discrete scroll values are in multiples of 120, as expected by amount_v120.
        Some(tuple_axis(self.discrete?, axis) as f64)
    }
}

fn tuple_axis<T>(tuple: (T, T), axis: Axis) -> T {
    match axis {
        Axis::Horizontal => tuple.0,
        Axis::Vertical => tuple.1,
    }
}

impl PointerAxisEvent<EisInputBackend> for EisEventAdapter<reis::request::Frame, ScrollFrame> {
    // FIXME: contradiction:
    // - `fn amount` documentation says that this is in pixels
    // - EI protocol says that a single scroll notch is 1.0
    // - Niri converts v120 to delta with (x/120*15)
    fn amount(&self, axis: Axis) -> Option<f64> {
        self.extra.amount(axis)
    }

    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        self.extra.amount_v120(axis)
    }

    fn source(&self) -> AxisSource {
        // No source for this, can only guess
        if self.extra.delta.is_some() || self.extra.stop.is_some() {
            AxisSource::Continuous
        } else {
            AxisSource::Wheel
        }
    }

    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

impl PointerButtonEvent<EisInputBackend> for EisEventAdapter<reis::request::Button> {
    fn button_code(&self) -> u32 {
        self.inner.button
    }

    fn state(&self) -> ButtonState {
        match self.inner.state {
            reis::ei::button::ButtonState::Released => ButtonState::Released,
            reis::ei::button::ButtonState::Press => ButtonState::Pressed,
        }
    }
}

impl PointerMotionEvent<EisInputBackend> for EisEventAdapter<reis::request::PointerMotion> {
    fn delta_x(&self) -> f64 {
        self.inner.dx as f64
    }

    fn delta_y(&self) -> f64 {
        self.inner.dy as f64
    }

    // Virtual pointer impl does this
    fn delta_x_unaccel(&self) -> f64 {
        self.inner.dx as f64
    }

    fn delta_y_unaccel(&self) -> f64 {
        self.inner.dy as f64
    }
}

pub struct AbsolutePositionEventExtra {
    /// Rectangle that covers all [EI regions](reis::event::Region) advertised to the EI client.
    pub regions_extent: Rectangle<f64, Logical>,
}

impl AbsolutePositionEvent<EisInputBackend>
    for EisEventAdapter<reis::request::PointerMotionAbsolute, AbsolutePositionEventExtra>
{
    fn x(&self) -> f64 {
        // Convert to unit interval as required by Niri
        (self.inner.dx_absolute as f64 - self.extra.regions_extent.loc.x)
            / self.extra.regions_extent.size.w
    }

    fn y(&self) -> f64 {
        (self.inner.dy_absolute as f64 - self.extra.regions_extent.loc.y)
            / self.extra.regions_extent.size.h
    }

    fn x_transformed(&self, width: i32) -> f64 {
        self.x() * width as f64
    }

    fn y_transformed(&self, height: i32) -> f64 {
        self.y() * height as f64
    }
}

impl PointerMotionAbsoluteEvent<EisInputBackend>
    for EisEventAdapter<reis::request::PointerMotionAbsolute, AbsolutePositionEventExtra>
{
}

//--------------//
// TOUCH EVENTS //
//--------------//

macro_rules! impl_touch_event {
    ($($item_name:path),+$(,)?) => {
        $(
        impl<Extra> TouchEvent<EisInputBackend> for EisEventAdapter<$item_name, Extra> {
            fn slot(&self) -> TouchSlot {
                Some(self.inner.touch_id).into()
            }
        }
        )+
    }
}

impl_touch_event!(
    reis::request::TouchDown,
    reis::request::TouchMotion,
    reis::request::TouchUp,
    reis::request::TouchCancel,
);

macro_rules! impl_touch_abspos {
    ($($item_name:path),+$(,)?) => {
        $(
        impl AbsolutePositionEvent<EisInputBackend> for EisEventAdapter<$item_name, AbsolutePositionEventExtra> {
            fn x(&self) -> f64 {
                // Convert to unit interval as required by Niri
                (self.inner.x as f64 - self.extra.regions_extent.loc.x) / self.extra.regions_extent.size.w
            }

            fn y(&self) -> f64 {
                (self.inner.y as f64 - self.extra.regions_extent.loc.y) / self.extra.regions_extent.size.h
            }

            fn x_transformed(&self, width: i32) -> f64 {
                self.x() * width as f64
            }

            fn y_transformed(&self, height: i32) -> f64 {
                self.y() * height as f64
            }
        }
        )+
    }
}

impl_touch_abspos!(reis::request::TouchDown, reis::request::TouchMotion);

impl TouchDownEvent<EisInputBackend>
    for EisEventAdapter<reis::request::TouchDown, AbsolutePositionEventExtra>
{
}
impl TouchMotionEvent<EisInputBackend>
    for EisEventAdapter<reis::request::TouchMotion, AbsolutePositionEventExtra>
{
}
impl TouchUpEvent<EisInputBackend> for EisEventAdapter<reis::request::TouchUp> {}
impl TouchCancelEvent<EisInputBackend> for EisEventAdapter<reis::request::TouchCancel> {}

pub struct TouchFrame;

impl TouchFrameEvent<EisInputBackend> for EisEventAdapter<reis::request::Frame, TouchFrame> {}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_delta_does_not_emit_vertical_amount() {
        let frame = ScrollFrame {
            delta: Some((2.5, 0.0)),
            ..Default::default()
        };

        assert_eq!(frame.amount(Axis::Horizontal), Some(2.5));
        assert_eq!(frame.amount(Axis::Vertical), None);
    }

    #[test]
    fn stop_emits_zero_only_for_the_stopped_axis() {
        let frame = ScrollFrame {
            stop: Some(((true, false), false)),
            ..Default::default()
        };

        assert_eq!(frame.amount(Axis::Horizontal), Some(0.0));
        assert_eq!(frame.amount(Axis::Vertical), None);
    }

    #[test]
    fn standalone_stop_survives_without_delta() {
        let frame = ScrollFrame {
            stop: Some(((false, true), false)),
            ..Default::default()
        };

        assert_eq!(frame.amount(Axis::Horizontal), None);
        assert_eq!(frame.amount(Axis::Vertical), Some(0.0));
    }

    #[test]
    fn discrete_amounts_preserve_raw_v120_values() {
        let frame = ScrollFrame {
            discrete: Some((240, -120)),
            ..Default::default()
        };

        assert_eq!(frame.amount_v120(Axis::Horizontal), Some(240.0));
        assert_eq!(frame.amount_v120(Axis::Vertical), Some(-120.0));
    }
}
