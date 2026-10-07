//! Module containing the parts of the implementation of remote desktop that need global state,
//! including EIS (=emulated input server).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use calloop::RegistrationToken;
use enumflags2::BitFlags;
use reis::calloop::{EisRequestSource, EisRequestSourceEvent};
use reis::ei::device::DeviceType;
use reis::ei::keyboard::KeymapType;
use reis::eis;
use reis::event::DeviceCapability;
use reis::request::{Device as EiDevice, EisRequest, Seat as EiSeat};
use smithay::backend::input::{InputTime, KeyState, Keycode};
use smithay::input::keyboard::{xkb, KeymapFile, Keysym};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::backend::IpcOutputMap;
use crate::dbus::mutter_remote_desktop::{MutterXdpDeviceType, RemoteDesktopDBusToCalloop};
use crate::input::dbus_remote_desktop_backend::{
    RdEventAdapter, RdInputBackend, RdKeyboardKeyEvent,
};
use crate::input::eis_backend::{
    AbsolutePositionEventExtra, EisEventAdapter, EisInputBackend, PressedCount, ScrollFrame,
    TouchFrame,
};
use crate::niri::State;
use crate::utils::{get_monotonic_time, global_bounding_rectangle_ipc, RemoteDesktopSessionId};

/// Processes an input event with the EIS event adapter.
macro_rules! process_event {
    ($global_state:expr, $session_id:expr, $inner:expr, $variant:ident) => {{
        process_event!(
            $global_state,
            $session_id,
            $inner,
            $variant,
            (),
            smithay::input::keyboard::KeyboardSource::MAIN
        )
    }};
    ($global_state:expr, $session_id:expr, $inner:expr, $variant:ident, $extra:expr) => {{
        process_event!(
            $global_state,
            $session_id,
            $inner,
            $variant,
            $extra,
            smithay::input::keyboard::KeyboardSource::MAIN
        )
    }};
    ($global_state:expr, $session_id:expr, $inner:expr, $variant:ident, $extra:expr, $source:expr) => {{
        $global_state.process_input_event(InputEvent::$variant {
            event: EisEventAdapter {
                session_id: $session_id,
                source: $source,
                inner: $inner,
                extra: $extra,
            },
        });
    }};
}

type InputEvent = smithay::backend::input::InputEvent<EisInputBackend>;

/// Child struct of the global [`State`] struct
#[derive(Default)]
pub struct RemoteDesktopState {
    /// Active EI sessions.
    active_ei_sessions: HashMap<RemoteDesktopSessionId, ConnectionState>,

    /// Counts the number of remote desktop sessions requiring touch capability on the seat.
    ///
    /// Modified by [`crate::dbus::mutter_remote_desktop`] (D-Bus).
    pub dbus_touch_session_counter: usize,
    /// Current focused output name, shared with the screencast D-Bus service.
    pub selected_output: Arc<Mutex<Option<String>>>,
    active_sessions: HashSet<RemoteDesktopSessionId>,
    session_close_sender: Option<async_channel::Sender<RemoteDesktopSessionId>>,
    /// Modifier keys synthesized for active D-Bus TextKeysym inputs, keyed by session and keysym.
    keysym_modifiers: HashMap<
        (RemoteDesktopSessionId, u32),
        (smithay::input::keyboard::KeyboardSource, u32, Vec<u32>),
    >,
    /// Input devices that currently hold each pointer button.
    pointer_button_origins: HashMap<u32, HashSet<String>>,
    /// Maps all input device touch slots into one calloop-wide slot space.
    touch_slots:
        HashMap<(String, smithay::backend::input::TouchSlot), smithay::backend::input::TouchSlot>,
    /// Raw touch slots that currently have an active contact, grouped by device.
    active_touch_slots: HashMap<String, HashSet<smithay::backend::input::TouchSlot>>,
    next_touch_slot: u32,
}

impl RemoteDesktopState {
    pub(crate) fn touch_slot(
        &mut self,
        device_id: String,
        slot: smithay::backend::input::TouchSlot,
    ) -> smithay::backend::input::TouchSlot {
        match self.touch_slots.entry((device_id, slot)) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mapped = Some(self.next_touch_slot).into();
                self.next_touch_slot = self.next_touch_slot.wrapping_add(1);
                entry.insert(mapped);
                mapped
            }
        }
    }

    pub(crate) fn activate_touch_slot(
        &mut self,
        device_id: String,
        slot: smithay::backend::input::TouchSlot,
    ) {
        self.active_touch_slots
            .entry(device_id)
            .or_default()
            .insert(slot);
    }

    pub(crate) fn release_touch_slot(
        &mut self,
        device_id: String,
        slot: smithay::backend::input::TouchSlot,
    ) {
        self.touch_slots.remove(&(device_id.clone(), slot));
        if let Some(slots) = self.active_touch_slots.get_mut(&device_id) {
            slots.remove(&slot);
            if slots.is_empty() {
                self.active_touch_slots.remove(&device_id);
            }
        }
    }

    /// Returns the mapped active slots for one device, then drops that device's state.
    pub(crate) fn cancel_touch_slots(
        &mut self,
        device_id: &str,
    ) -> Vec<smithay::backend::input::TouchSlot> {
        let active_slots = self
            .active_touch_slots
            .remove(device_id)
            .unwrap_or_default();
        let slots = self
            .touch_slots
            .iter()
            .filter_map(|((origin, slot), mapped)| {
                (origin == device_id && active_slots.contains(slot)).then_some(*mapped)
            })
            .collect();
        self.touch_slots
            .retain(|(origin, _), _| origin != device_id);
        slots
    }

    pub(crate) fn cancel_touch_slot(
        &mut self,
        device_id: &str,
        slot: smithay::backend::input::TouchSlot,
    ) -> Option<smithay::backend::input::TouchSlot> {
        let mapped = self.touch_slots.remove(&(device_id.to_owned(), slot))?;
        if let Some(slots) = self.active_touch_slots.get_mut(device_id) {
            slots.remove(&slot);
            if slots.is_empty() {
                self.active_touch_slots.remove(device_id);
            }
        }
        Some(mapped)
    }

    pub fn set_session_close_sender(
        &mut self,
        sender: async_channel::Sender<RemoteDesktopSessionId>,
    ) {
        self.session_close_sender = Some(sender);
    }
    /// Returns whether this origin changed the aggregate pressed state for a pointer button.
    pub(crate) fn update_pointer_button_origin(
        &mut self,
        button: u32,
        origin: String,
        pressed: bool,
    ) -> bool {
        if pressed {
            let origins = self.pointer_button_origins.entry(button).or_default();
            let was_empty = origins.is_empty();
            origins.insert(origin) && was_empty
        } else {
            let Some(origins) = self.pointer_button_origins.get_mut(&button) else {
                return false;
            };
            if !origins.remove(&origin) || !origins.is_empty() {
                return false;
            }
            self.pointer_button_origins.remove(&button);
            true
        }
    }

    /// Whether touch capability on the seat is needed.
    pub fn needs_touch_cap(&self) -> bool {
        self.dbus_touch_session_counter > 0
            || self
                .active_ei_sessions
                .values()
                .any(|sess| sess.needs_touch_cap())
    }

    fn set_session_active(&mut self, session_id: RemoteDesktopSessionId, active: bool) {
        if active {
            self.active_sessions.insert(session_id);
        } else {
            self.active_sessions.remove(&session_id);
        }
    }

    fn input_is_authorized(&self, session_id: RemoteDesktopSessionId) -> bool {
        self.active_sessions.contains(&session_id)
    }
    pub fn set_selected_output(&self, name: Option<String>) {
        if let Ok(mut selected_output) = self.selected_output.lock() {
            *selected_output = name;
        }
    }
}

struct PendingKeyboardKey {
    event: reis::request::KeyboardKey,
    source: smithay::input::keyboard::KeyboardSource,
}

/// The state for an EI connection (not the `ei_connection` object but
/// [`Context`](reis::eis::Context)).
struct ConnectionState {
    last_capabilities: Option<BitFlags<DeviceCapability, u64>>,
    ei_connection: Option<reis::request::Connection>,
    seat: Option<EiSeat>,
    /// The number of keys pressed on devices in this connection.
    key_counter: u32,
    exposed_device_types: BitFlags<MutterXdpDeviceType>,
    session_id: RemoteDesktopSessionId,
    /// Keyboard source for the keyboard device in this session.
    keyboard_source: Option<smithay::input::keyboard::KeyboardSource>,
    /// Scroll values received for each device until its next frame.
    scroll_frames: HashMap<reis::request::Device, ScrollFrame>,
    /// Keyboard events received for each device until its next frame.
    pending_keyboard: HashMap<reis::request::Device, Vec<PendingKeyboardKey>>,
    /// Per-keysym source and generated keys held until matching release or teardown.
    keysym_modifiers: HashMap<
        (reis::request::Device, u32),
        (smithay::input::keyboard::KeyboardSource, u32, Vec<u32>),
    >,
    /// Devices that have touch events awaiting their EIS frame request.
    next_frame_touch: HashSet<reis::request::Device>,

    /// Keys currently held by this session, including the source that owns each press.
    held_keys: HashSet<(
        reis::request::Device,
        u32,
        smithay::input::keyboard::KeyboardSource,
    )>,
    /// Pointer buttons currently held by this session.
    held_buttons: HashSet<(reis::request::Device, u32)>,
    /// Touch slots currently held by this session.
    held_touches: HashSet<(reis::request::Device, u32)>,

    // Stored for e.g. reload integration
    keyboard_device: Option<EiDevice>,
    mouse_device: Option<EiDevice>,
    touch_device: Option<EiDevice>,

    event_loop_token: RegistrationToken,
}
impl ConnectionState {
    /// Whether this session requires touch capabilities on the seat.
    fn needs_touch_cap(&self) -> bool {
        self.touch_device.is_some()
    }

    /// Returns the rectangle that covers all [EI regions](reis::event::Region) advertised to the EI
    /// client.
    ///
    /// Ignores location as we offset it to 0 in [`advertise_regions`].
    fn regions_extent(&self, backend: &crate::backend::Backend) -> Option<Rectangle<f64, Logical>> {
        let ipc_outputs = backend.ipc_outputs();
        let ipc_outputs = ipc_outputs.lock().unwrap();

        let global_extent = global_bounding_rectangle_ipc(&ipc_outputs)?;

        Some(Rectangle::new(
            // EI doesn't allow negative positions so this is offset to 0 when advertising
            // `ei_region`s.
            Point::default(),
            Size::new(global_extent.size.w as f64, global_extent.size.h as f64),
        ))
    }
}

impl State {
    pub fn on_ipc_outputs_changed_remote_desktop(&mut self) {
        // recreate EI devices
        let cloned: Vec<_> = self
            .niri
            .remote_desktop
            .active_ei_sessions
            .values()
            .filter_map(|sess| {
                Some((
                    sess.session_id,
                    sess.ei_connection.clone()?,
                    sess.seat.clone()?,
                    sess.last_capabilities?,
                ))
            })
            .collect();

        for (session_id, ei_conn, seat, last_caps) in cloned {
            create_ei_devices(&ei_conn, self, session_id, seat, last_caps);
        }
    }

    pub fn on_remote_desktop_msg_from_dbus(&mut self, msg: RemoteDesktopDBusToCalloop) {
        match msg {
            RemoteDesktopDBusToCalloop::RemoveEisHandler { session_id } => {
                self.remove_eis_session(session_id, true, false);
            }
            RemoteDesktopDBusToCalloop::NewEisContext {
                session_id,
                ctx,
                exposed_device_types,
                setup_result,
            } => {
                let _ = setup_result.send_blocking(self.create_new_eis_context(
                    session_id,
                    ctx,
                    exposed_device_types,
                ));
            }

            RemoteDesktopDBusToCalloop::EmulateInput {
                session_id,
                event,
                release,
            } => {
                if release || self.niri.remote_desktop.input_is_authorized(session_id) {
                    self.process_input_event(event);
                }
            }
            RemoteDesktopDBusToCalloop::SetSessionActive { session_id, active } => {
                self.niri
                    .remote_desktop
                    .set_session_active(session_id, active);
                if !active {
                    self.release_keysym_modifiers_for_session(session_id);
                }
            }
            RemoteDesktopDBusToCalloop::EmulateKeysym {
                keysym,
                state,
                session_id,
                time,
                keyboard_source,
                release,
            } => {
                if !release && !self.niri.remote_desktop.input_is_authorized(session_id) {
                    return;
                }
                let keysym = Keysym::from(keysym);

                let pressed = state == KeyState::Pressed;
                let tracked = if pressed {
                    self.niri
                        .remote_desktop
                        .keysym_modifiers
                        .get(&(session_id, keysym.raw()))
                        .cloned()
                } else {
                    self.niri
                        .remote_desktop
                        .keysym_modifiers
                        .remove(&(session_id, keysym.raw()))
                };
                let (session_source, target_keycode, modifier_keys) = if let Some(tracked) = tracked
                {
                    tracked
                } else {
                    let Some(keyboard_handle) = self.niri.seat.get_keyboard() else {
                        warn!("TextKeysym received but seat has no keyboard");
                        return;
                    };
                    let mapped = keyboard_handle.with_xkb_state(self, |context| {
                        let xkb = context.xkb().lock().unwrap();
                        // SAFETY: the state's ref count isn't increased
                        let xkb_state = unsafe { xkb.state() };
                        // SAFETY: the keymap's ref count isn't increased
                        let keymap = unsafe { xkb.keymap() };
                        let layout_index = xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
                        keysym_to_keycode(xkb_state, keymap, keysym).map(|(keycode, mod_mask)| {
                            (
                                keycode.raw(),
                                mod_mask,
                                modifier_keycodes(keymap, mod_mask, layout_index),
                            )
                        })
                    });
                    let Some((target_keycode, _mod_mask, modifiers)) = mapped else {
                        warn!(
                            "Couldn't find keycode for keysym {} (raw {}) in the current keyboard layout",
                            keysym.name().unwrap_or_default(),
                            keysym.raw()
                        );
                        return;
                    };
                    let Some(modifiers) = modifiers else {
                        error!(
                            "Rejecting text keysym {} because it requires unsupported XKB modifiers",
                            keysym.name().unwrap_or_default()
                        );
                        return;
                    };
                    let source = if pressed {
                        smithay::input::keyboard::KeyboardSource::new_auxiliary()
                    } else {
                        keyboard_source
                    };
                    let tracked = (source, target_keycode, modifiers);
                    if pressed {
                        self.niri
                            .remote_desktop
                            .keysym_modifiers
                            .insert((session_id, keysym.raw()), tracked.clone());
                    }
                    tracked
                };

                debug!(
                    "{} Emulating keysym={:12} X11 keycode={: <3}",
                    if pressed { "╭" } else { "╰" },
                    keysym.name().unwrap_or_default(),
                    target_keycode,
                );

                let mut emit_key = |keycode| {
                    self.process_input_event::<RdInputBackend>(
                        smithay::backend::input::InputEvent::Keyboard {
                            event: RdEventAdapter {
                                session_id,
                                source: session_source,
                                time,
                                inner: RdKeyboardKeyEvent { keycode, state },
                            },
                        },
                    );
                };
                let keycode = Keycode::new(target_keycode);
                if pressed {
                    for modifier in modifier_keys {
                        emit_key(Keycode::new(modifier + 8));
                    }
                    emit_key(keycode);
                } else {
                    emit_key(keycode);
                    for modifier in modifier_keys.into_iter().rev() {
                        emit_key(Keycode::new(modifier + 8));
                    }
                }
            }
            RemoteDesktopDBusToCalloop::IncTouchSession => {
                self.niri.remote_desktop.dbus_touch_session_counter += 1;

                self.refresh_wayland_device_caps();
            }
            RemoteDesktopDBusToCalloop::DecTouchSession => {
                self.niri.remote_desktop.dbus_touch_session_counter = self
                    .niri
                    .remote_desktop
                    .dbus_touch_session_counter
                    .saturating_sub(1);

                self.refresh_wayland_device_caps();
            }
        }
    }

    fn remove_eis_session(
        &mut self,
        session_id: RemoteDesktopSessionId,
        remove_source: bool,
        invalidate_session: bool,
    ) {
        self.niri
            .remote_desktop
            .set_session_active(session_id, false);
        self.release_keysym_modifiers_for_session(session_id);

        if let Some(mut session) = self
            .niri
            .remote_desktop
            .active_ei_sessions
            .remove(&session_id)
        {
            if remove_source {
                self.niri.event_loop.remove(session.event_loop_token);
            }

            self.release_eis_input(session_id, &session);

            for device in [
                &mut session.keyboard_device,
                &mut session.mouse_device,
                &mut session.touch_device,
            ]
            .into_iter()
            .flatten()
            {
                device.remove();
            }
            self.refresh_wayland_device_caps();
        }

        if invalidate_session {
            if let Some(sender) = &self.niri.remote_desktop.session_close_sender {
                if sender.try_send(session_id).is_err() {
                    warn!("Could not queue remote desktop session invalidation: calloop-to-D-Bus channel is closed");
                }
            }
        }
    }

    fn release_keysym_modifiers_for_session(&mut self, session_id: RemoteDesktopSessionId) {
        let generated = {
            let state = &mut self.niri.remote_desktop.keysym_modifiers;
            let mut generated = Vec::new();
            state.retain(|(session, _), (source, keycode, modifiers)| {
                if *session == session_id {
                    generated.push((*source, *keycode, modifiers.clone()));
                    false
                } else {
                    true
                }
            });
            generated
        };
        let time = get_monotonic_time().as_micros().min(u64::MAX as u128) as u64;
        for (source, keycode, modifiers) in generated {
            for keycode in std::iter::once(keycode)
                .chain(modifiers.into_iter().rev().map(|modifier| modifier + 8))
            {
                self.process_input_event::<RdInputBackend>(
                    smithay::backend::input::InputEvent::Keyboard {
                        event: RdEventAdapter {
                            session_id,
                            source,
                            time: InputTime::from_micros(time),
                            inner: RdKeyboardKeyEvent {
                                keycode: Keycode::new(keycode),
                                state: KeyState::Released,
                            },
                        },
                    },
                );
            }
        }
    }

    fn release_eis_input(&mut self, session_id: RemoteDesktopSessionId, session: &ConnectionState) {
        let source = session
            .keyboard_source
            .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
        let time = get_monotonic_time().as_micros().min(u64::MAX as u128) as u64;
        let mut key_count = session.key_counter;

        for (device, key, key_source) in &session.held_keys {
            key_count = key_count.saturating_sub(1);
            self.process_input_event(InputEvent::Keyboard {
                event: EisEventAdapter {
                    session_id,
                    source: *key_source,
                    inner: reis::request::KeyboardKey {
                        device: device.clone(),
                        time,
                        key: *key,
                        state: reis::ei::keyboard::KeyState::Released,
                    },
                    extra: PressedCount(key_count),
                },
            });
        }

        for (device, button) in &session.held_buttons {
            self.process_input_event(InputEvent::PointerButton {
                event: EisEventAdapter {
                    session_id,
                    source,
                    inner: reis::request::Button {
                        device: device.clone(),
                        time,
                        button: *button,
                        state: reis::ei::button::ButtonState::Released,
                    },
                    extra: (),
                },
            });
        }

        let mut touch_devices = HashSet::new();
        for (device, touch_id) in &session.held_touches {
            self.process_input_event(InputEvent::TouchUp {
                event: EisEventAdapter {
                    session_id,
                    source,
                    inner: reis::request::TouchUp {
                        device: device.clone(),
                        time,
                        touch_id: *touch_id,
                    },
                    extra: (),
                },
            });
            touch_devices.insert(device.clone());
        }
        for device in touch_devices {
            self.process_input_event(InputEvent::TouchFrame {
                event: EisEventAdapter {
                    session_id,
                    source,
                    inner: reis::request::Frame {
                        device,
                        last_serial: 0,
                        time,
                    },
                    extra: TouchFrame,
                },
            });
        }
    }
    /// Releases all held input (keys, buttons, touches) for a specific EIS device
    /// within a session. Used when the client closes a device or stops emulating.
    fn release_eis_device_input(
        &mut self,
        session_id: RemoteDesktopSessionId,
        device: &reis::request::Device,
    ) {
        let Some((keyboard_source, key_counter, held_keys, held_buttons, held_touches)) = self
            .niri
            .remote_desktop
            .active_ei_sessions
            .get(&session_id)
            .map(|session| {
                (
                    session.keyboard_source,
                    session.key_counter,
                    session.held_keys.iter().cloned().collect::<Vec<_>>(),
                    session.held_buttons.iter().cloned().collect::<Vec<_>>(),
                    session.held_touches.iter().cloned().collect::<Vec<_>>(),
                )
            })
        else {
            return;
        };
        let keyboard_source =
            keyboard_source.unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
        let time = get_monotonic_time().as_micros().min(u64::MAX as u128) as u64;
        let mut key_count = key_counter;

        for (held_device, key, source) in held_keys {
            if held_device == *device {
                key_count = key_count.saturating_sub(1);
                self.process_input_event(InputEvent::Keyboard {
                    event: EisEventAdapter {
                        session_id,
                        source,
                        inner: reis::request::KeyboardKey {
                            device: held_device,
                            time,
                            key,
                            state: reis::ei::keyboard::KeyState::Released,
                        },
                        extra: PressedCount(key_count),
                    },
                });
            }
        }

        for (held_device, button) in held_buttons {
            if held_device == *device {
                self.process_input_event(InputEvent::PointerButton {
                    event: EisEventAdapter {
                        session_id,
                        source: keyboard_source,
                        inner: reis::request::Button {
                            device: held_device,
                            time,
                            button,
                            state: reis::ei::button::ButtonState::Released,
                        },
                        extra: (),
                    },
                });
            }
        }

        let mut touch_devices = HashSet::new();
        for (held_device, touch_id) in held_touches {
            if held_device == *device {
                self.process_input_event(InputEvent::TouchUp {
                    event: EisEventAdapter {
                        session_id,
                        source: keyboard_source,
                        inner: reis::request::TouchUp {
                            device: held_device.clone(),
                            time,
                            touch_id,
                        },
                        extra: (),
                    },
                });
                touch_devices.insert(held_device);
            }
        }
        for held_device in touch_devices {
            self.process_input_event(InputEvent::TouchFrame {
                event: EisEventAdapter {
                    session_id,
                    source: keyboard_source,
                    inner: reis::request::Frame {
                        device: held_device,
                        last_serial: 0,
                        time,
                    },
                    extra: TouchFrame,
                },
            });
        }
    }
    fn create_new_eis_context(
        &mut self,
        session_id: RemoteDesktopSessionId,
        ctx: eis::Context,
        exposed_device_types: BitFlags<MutterXdpDeviceType, u32>,
    ) -> Result<(), String> {
        if !self.niri.remote_desktop.input_is_authorized(session_id) {
            return Err("Remote desktop session is no longer active".to_owned());
        }

        let event_loop_token = match self.niri.event_loop.insert_source(
            EisRequestSource::new(ctx, 1),
            move |event, connection, state| {
                let mut post_action =
                    handle_eis_request_source_event(event, connection, state, session_id);
                if post_action != calloop::PostAction::Continue {
                    debug!("EIS connection {post_action:?}");
                }
                if let Err(err) = connection.flush() {
                    warn!("Error while flushing connection: {err}");
                    post_action = calloop::PostAction::Remove
                }

                if matches!(
                    post_action,
                    calloop::PostAction::Remove | calloop::PostAction::Disable
                ) {
                    state.remove_eis_session(session_id, false, true);
                }

                // Always Ok because we never want to propagate the error
                // out of the entire event loop
                Ok(post_action)
            },
        ) {
            Ok(token) => token,
            Err(err) => {
                warn!("Error inserting EIS source: {err:?}");
                self.remove_eis_session(session_id, true, true);
                return Err(format!("Error inserting EIS source: {err}"));
            }
        };

        let conn_state = ConnectionState {
            last_capabilities: None,
            ei_connection: None,
            seat: None,
            key_counter: 0,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            held_touches: HashSet::new(),
            exposed_device_types,
            session_id,
            keyboard_source: None,
            scroll_frames: HashMap::new(),
            pending_keyboard: HashMap::new(),
            keysym_modifiers: HashMap::new(),
            next_frame_touch: HashSet::new(),
            keyboard_device: None,
            mouse_device: None,
            touch_device: None,
            event_loop_token,
        };

        self.niri
            .remote_desktop
            .active_ei_sessions
            .insert(session_id, conn_state);
        Ok(())
    }
}

fn handle_eis_request_source_event(
    event: Result<EisRequestSourceEvent, reis::Error>,
    connection: &mut reis::request::Connection,
    global_state: &mut State,
    session_id: RemoteDesktopSessionId,
) -> calloop::PostAction {
    if !global_state
        .niri
        .remote_desktop
        .input_is_authorized(session_id)
    {
        return calloop::PostAction::Remove;
    }
    match event {
        Ok(event) => match event {
            EisRequestSourceEvent::Connected => {
                debug!("EIS connected!");
                if !connection.has_interface("ei_seat") || !connection.has_interface("ei_device") {
                    connection.disconnected(
                        eis::connection::DisconnectReason::Protocol,
                        Some("Need `ei_seat` and `ei_device`"),
                    );
                    return calloop::PostAction::Remove;
                }

                let conn_state: &mut ConnectionState = global_state
                    .niri
                    .remote_desktop
                    .active_ei_sessions
                    .get_mut(&session_id)
                    .expect("remote desktop session being processed should exist");

                let seat = connection.add_seat(
                    Some("default"),
                    MutterXdpDeviceType::to_reis_capabilities(conn_state.exposed_device_types),
                );

                conn_state.seat = Some(seat);

                calloop::PostAction::Continue
            }
            EisRequestSourceEvent::Request(request) => {
                debug!("EIS request! {:#?}", request);
                handle_eis_request(request, connection, global_state, session_id)
            }
        },
        Err(err) => {
            warn!("EIS protocol error: {err}");
            connection.disconnected(
                eis::connection::DisconnectReason::Protocol,
                Some(&err.to_string()),
            );
            calloop::PostAction::Remove
        }
    }
}

// TODO: send ei_keyboard.modifiers when other keyboards change modifier state?
// TODO: recreate keyboard with new keymaps
// ^ Waiting for https://github.com/Smithay/smithay/issues/1776

/// Creates an EI keyboard if the capabilities match.
///
/// The device must be [`EiDevice::resumed`] for clients to request to
/// [`EisRequest::DeviceStartEmulating`] input.
fn create_ei_keyboard(
    seat: &EiSeat,
    capabilities: BitFlags<DeviceCapability>,
    connection: &reis::request::Connection,
    global_state: &mut State,
) -> Option<EiDevice> {
    (capabilities.contains(DeviceCapability::Keyboard) && connection.has_interface("ei_keyboard"))
        .then(|| {
            seat.add_device(
                Some("keyboard"),
                DeviceType::Virtual,
                DeviceCapability::Keyboard.into(),
                |device| {
                    let keyboard: reis::eis::Keyboard = device
                        .interface()
                        .expect("Should exist because it was just defined");

                    let file = global_state
                        .niri
                        .seat
                        .get_keyboard()
                        .unwrap()
                        .with_xkb_state(global_state, |context| {
                            let xkb = context.xkb().lock().unwrap();

                            // SAFETY: the keymap's ref count isn't increased
                            let keymap = unsafe { xkb.keymap() };
                            KeymapFile::new(keymap)
                        });

                    // > The fd must be mapped with MAP_PRIVATE by the recipient, as MAP_SHARED may fail.
                    //
                    // EI protocol allows us to use anonymous, sealed files.
                    file.with_fd(true, |fd, size| {
                        // Smithay also does this cast
                        keyboard.keymap(KeymapType::Xkb, size as u32, fd);
                    })
                    .unwrap();
                    debug!("Sent keymap file");

                    let ipc_outputs = global_state.backend.ipc_outputs();
                    let ipc_outputs = ipc_outputs.lock().unwrap();
                    advertise_regions(device, &ipc_outputs);
                },
            )
        })
}

/// Creates an EI mouse if the capabilities match.
///
/// The device must be [`EiDevice::resumed`] for clients to request to
/// [`EisRequest::DeviceStartEmulating`] input.
fn create_ei_mouse(
    seat: &EiSeat,
    capabilities: BitFlags<DeviceCapability>,
    connection: &reis::request::Connection,
    ipc_outputs: &IpcOutputMap,
) -> Option<EiDevice> {
    let mut mouse_capabilities = BitFlags::empty();

    let mut check_mouse_cap = |capability, interface| {
        // We check for the interfaces' existence because the client may send
        // a 0xffffffffffffffff and then any events we send to the sub-interfaces will be
        // protocol violations.
        if capabilities.contains(capability) && connection.has_interface(interface) {
            mouse_capabilities |= capability;
        }
    };

    check_mouse_cap(DeviceCapability::Pointer, "ei_pointer");
    check_mouse_cap(DeviceCapability::Scroll, "ei_scroll");
    check_mouse_cap(DeviceCapability::Button, "ei_button");
    check_mouse_cap(DeviceCapability::PointerAbsolute, "ei_pointer_absolute");

    (!mouse_capabilities.is_empty()).then(|| {
        seat.add_device(
            Some("mouse"),
            DeviceType::Virtual,
            mouse_capabilities,
            |device| {
                advertise_regions(device, ipc_outputs);
            },
        )
    })
}

/// Creates an EI keyboard if the capabilities match.
///
/// The device must be [`EiDevice::resumed`] for clients to request to
/// [`EisRequest::DeviceStartEmulating`] input.
fn create_ei_touchscreen(
    seat: &EiSeat,
    capabilities: BitFlags<DeviceCapability>,
    connection: &reis::request::Connection,
    ipc_outputs: &IpcOutputMap,
) -> Option<EiDevice> {
    (capabilities.contains(DeviceCapability::Touch) && connection.has_interface("ei_touchscreen"))
        .then(|| {
            seat.add_device(
                Some("touchscreen"),
                DeviceType::Virtual,
                DeviceCapability::Touch.into(),
                |device| {
                    advertise_regions(device, ipc_outputs);
                },
            )
        })
}

/// (Re)creates EI devices.
fn create_ei_devices(
    connection: &reis::request::Connection,
    global_state: &mut State,
    session_id: RemoteDesktopSessionId,
    seat: EiSeat,
    capabilities: BitFlags<DeviceCapability, u64>,
) {
    macro_rules! get_conn_state {
        // global_state is explicitly specified as a reminder for the lifetime stuff.
        ($global_state: ident) => {
            $global_state
                .niri
                .remote_desktop
                .active_ei_sessions
                .get_mut(&session_id)
                .expect("remote desktop session being processed should exist")
        };
    }

    // Release held input through the old virtual devices and their keyboard source
    // before removing the devices or replacing the source.
    let old_devices = {
        let conn_state = get_conn_state!(global_state);
        [
            conn_state.keyboard_device.as_ref(),
            conn_state.mouse_device.as_ref(),
            conn_state.touch_device.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(|device| device.clone())
        .collect::<Vec<_>>()
    };
    for device in &old_devices {
        global_state.release_eis_device_input(session_id, device);
    }

    {
        let conn_state = get_conn_state!(global_state);
        for device in &old_devices {
            conn_state.held_keys.retain(|(held, _, _)| held != device);
            conn_state.held_buttons.retain(|(held, _)| held != device);
            conn_state.held_touches.retain(|(held, _)| held != device);
            conn_state.pending_keyboard.remove(device);
            conn_state.scroll_frames.remove(device);
            conn_state.next_frame_touch.remove(device);
            conn_state
                .keysym_modifiers
                .retain(|(held, _), _| held != device);
        }
        conn_state.key_counter = conn_state.held_keys.len().min(u32::MAX as usize) as u32;

        for old_device_slot in [
            &mut conn_state.keyboard_device,
            &mut conn_state.mouse_device,
            &mut conn_state.touch_device,
        ]
        .into_iter()
        {
            if let Some(old_device) = old_device_slot {
                old_device.remove();
                *old_device_slot = None;
            }
        }
        conn_state.keyboard_source = None;
    }

    // This is funnily separated like this because of the mutable aliasing of conn_state and
    // global_state
    let keyboard_device = create_ei_keyboard(&seat, capabilities, connection, global_state);

    {
        let conn_state = get_conn_state!(global_state);

        let ipc_outputs = global_state.backend.ipc_outputs();
        let ipc_outputs = ipc_outputs.lock().unwrap();

        if let Some(device) = keyboard_device {
            device.resumed();
            let keyboard_source = smithay::input::keyboard::KeyboardSource::new_auxiliary();
            conn_state.keyboard_source = Some(keyboard_source);
            conn_state.keyboard_device = Some(device);
        }

        if let Some(device) = create_ei_mouse(&seat, capabilities, connection, &ipc_outputs) {
            device.resumed();
            conn_state.mouse_device = Some(device);
        }

        if let Some(device) = create_ei_touchscreen(&seat, capabilities, connection, &ipc_outputs) {
            device.resumed();
            conn_state.touch_device = Some(device);
        }
    }

    // Update the Wayland devices based on the stored data
    global_state.refresh_wayland_device_caps();
}

/// Advertises regions on EI devices.
fn advertise_regions(device: &EiDevice, ipc_outputs: &IpcOutputMap) {
    let Some(bounding_rect) = global_bounding_rectangle_ipc(ipc_outputs) else {
        return;
    };

    for output in ipc_outputs.values() {
        let Some(l) = output.logical else { continue };

        device.device().region_mapping_id(&output.name);

        device.device().region(
            // EI doesn't allow negative positions
            (l.x - bounding_rect.loc.x) as u32,
            (l.y - bounding_rect.loc.y) as u32,
            l.width,
            l.height,
            l.scale as f32,
        );
    }
}

fn handle_eis_request(
    request: reis::request::EisRequest,
    connection: &mut reis::request::Connection,
    global_state: &mut State,
    session_id: RemoteDesktopSessionId,
) -> calloop::PostAction {
    if !global_state
        .niri
        .remote_desktop
        .input_is_authorized(session_id)
        || !global_state
            .niri
            .remote_desktop
            .active_ei_sessions
            .contains_key(&session_id)
    {
        return calloop::PostAction::Remove;
    }

    macro_rules! get_conn_state {
        // global_state is explicitly specified as a reminder for the lifetime stuff.
        ($global_state: ident) => {
            $global_state
                .niri
                .remote_desktop
                .active_ei_sessions
                .get_mut(&session_id)
                .expect("remote desktop session being processed should exist")
        };
    }

    match request {
        EisRequest::Disconnect => {
            return calloop::PostAction::Remove;
        }
        EisRequest::Bind(reis::request::Bind { seat, capabilities }) => {
            let conn_state = get_conn_state!(global_state);
            if capabilities
                & MutterXdpDeviceType::to_reis_capabilities(conn_state.exposed_device_types)
                != capabilities
            {
                connection.disconnected(
                    eis::connection::DisconnectReason::Value,
                    Some("Binding to invalid capabilities"),
                );
                return calloop::PostAction::Remove;
            }

            conn_state.ei_connection = Some(connection.clone());
            conn_state.last_capabilities = Some(capabilities);

            // TODO: Why not combine everything into a single device?

            create_ei_devices(connection, global_state, session_id, seat, capabilities);
        }

        EisRequest::Ready(_) => {
            // The device is ready to receive events. Nothing to do; the device was
            // already set up and resumed in `create_ei_*`.
        }
        EisRequest::RequestDevice(_) => {
            // The client requested a device binding on the seat. The actual device
            // creation happens in `create_ei_devices` during `Bind`; this request is
            // only the seat-level bookkeeping, which we have nothing to do for.
        }
        EisRequest::DeviceStartEmulating(_) => {
            // Devices are already resumed when created during `Bind`.
        }
        EisRequest::DeviceClosed(inner) => {
            global_state.release_eis_device_input(session_id, &inner.device);
            let conn_state = get_conn_state!(global_state);
            // Drop the device from the session's held-input sets so a stale
            // reference can never be released again after teardown.
            conn_state
                .held_keys
                .retain(|(device, _, _)| device != &inner.device);
            conn_state
                .held_buttons
                .retain(|(device, _)| device != &inner.device);
            conn_state
                .held_touches
                .retain(|(device, _)| device != &inner.device);
            conn_state.pending_keyboard.remove(&inner.device);
            conn_state.scroll_frames.remove(&inner.device);
            conn_state.next_frame_touch.remove(&inner.device);
            conn_state
                .keysym_modifiers
                .retain(|(device, _), _| device != &inner.device);
            // Also drop the device from the stored device slots if it matches.
            for slot in [
                &mut conn_state.keyboard_device,
                &mut conn_state.mouse_device,
                &mut conn_state.touch_device,
            ]
            .into_iter()
            .flatten()
            {
                if &*slot == &inner.device {
                    slot.remove();
                }
            }
            inner.device.remove();
        }
        EisRequest::DeviceStopEmulating(inner) => {
            global_state.release_eis_device_input(session_id, &inner.device);
            let conn_state = get_conn_state!(global_state);
            // Drop the device from the session's held-input sets so a stale
            // reference can never be released again after teardown.
            conn_state
                .held_keys
                .retain(|(device, _, _)| device != &inner.device);
            conn_state
                .held_buttons
                .retain(|(device, _)| device != &inner.device);
            conn_state
                .held_touches
                .retain(|(device, _)| device != &inner.device);
            conn_state.pending_keyboard.remove(&inner.device);
            conn_state.scroll_frames.remove(&inner.device);
            conn_state.next_frame_touch.remove(&inner.device);
            conn_state
                .keysym_modifiers
                .retain(|(device, _), _| device != &inner.device);
        }
        EisRequest::TextKeysym(inner) => {
            let device = inner.device.clone();
            let pressed = inner.state == reis::ei::keyboard::KeyState::Press;
            let keysym_key = (device.clone(), inner.keysym);
            let tracked = {
                let conn_state = get_conn_state!(global_state);
                if pressed {
                    conn_state.keysym_modifiers.get(&keysym_key).cloned()
                } else {
                    conn_state.keysym_modifiers.remove(&keysym_key)
                }
            };

            let (key, modifiers, source) = if let Some((source, key, modifiers)) = tracked {
                (key, modifiers, source)
            } else {
                let Some(keyboard_handle) = global_state.niri.seat.get_keyboard() else {
                    warn!("TextKeysym received but seat has no keyboard");
                    return calloop::PostAction::Continue;
                };
                let mapped = keyboard_handle.with_xkb_state(global_state, |context| {
                    let xkb = context.xkb().lock().unwrap();
                    // SAFETY: the state's ref count isn't increased
                    let xkb_state = unsafe { xkb.state() };
                    // SAFETY: the keymap's ref count isn't increased
                    let keymap = unsafe { xkb.keymap() };
                    let layout_index = xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
                    keysym_to_keycode(xkb_state, keymap, Keysym::from(inner.keysym)).map(
                        |(keycode, mod_mask)| {
                            (
                                keycode.raw().saturating_sub(8),
                                modifier_keycodes(keymap, mod_mask, layout_index),
                            )
                        },
                    )
                });
                let Some((key, modifiers)) = mapped else {
                    warn!(
                        "Couldn't find keycode for text keysym {} in the current keyboard layout",
                        Keysym::from(inner.keysym).name().unwrap_or_default()
                    );
                    return calloop::PostAction::Continue;
                };
                let Some(modifiers) = modifiers else {
                    connection.disconnected(
                        eis::connection::DisconnectReason::Value,
                        Some("Text keysym requires unsupported XKB modifiers"),
                    );
                    return calloop::PostAction::Remove;
                };
                let source = if pressed {
                    smithay::input::keyboard::KeyboardSource::new_auxiliary()
                } else {
                    get_conn_state!(global_state)
                        .keyboard_source
                        .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN)
                };
                if pressed {
                    get_conn_state!(global_state)
                        .keysym_modifiers
                        .insert(keysym_key, (source, key, modifiers.clone()));
                }
                (key, modifiers, source)
            };

            let key_event = |key, state| PendingKeyboardKey {
                event: reis::request::KeyboardKey {
                    device: device.clone(),
                    time: inner.time,
                    key,
                    state,
                },
                source,
            };
            let pending = get_conn_state!(global_state)
                .pending_keyboard
                .entry(device.clone())
                .or_default();
            if pressed {
                pending.extend(
                    modifiers
                        .into_iter()
                        .map(|modifier| key_event(modifier, reis::ei::keyboard::KeyState::Press)),
                );
                pending.push(key_event(key, reis::ei::keyboard::KeyState::Press));
            } else {
                pending.push(key_event(key, reis::ei::keyboard::KeyState::Released));
                pending.extend(
                    modifiers.into_iter().rev().map(|modifier| {
                        key_event(modifier, reis::ei::keyboard::KeyState::Released)
                    }),
                );
            }
        }
        EisRequest::TextUtf8(_) => {
            connection.disconnected(
                eis::connection::DisconnectReason::Value,
                Some("Raw UTF-8 text injection is not supported"),
            );
            return calloop::PostAction::Remove;
        }

        EisRequest::PointerMotion(inner) => {
            process_event!(global_state, session_id, inner, PointerMotion)
        }

        EisRequest::PointerMotionAbsolute(inner) => {
            if let Some(regions_extent) =
                get_conn_state!(global_state).regions_extent(&global_state.backend)
            {
                process_event!(
                    global_state,
                    session_id,
                    inner,
                    PointerMotionAbsolute,
                    AbsolutePositionEventExtra { regions_extent }
                )
            };
        }

        EisRequest::Button(inner) => {
            let conn_state = get_conn_state!(global_state);
            update_held_input(
                &mut conn_state.held_buttons,
                inner.device.clone(),
                inner.button,
                inner.state == reis::ei::button::ButtonState::Press,
            );
            let source = conn_state
                .keyboard_source
                .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
            process_event!(global_state, session_id, inner, PointerButton, (), source)
        }

        EisRequest::ScrollDelta(inner) => {
            let scroll_frame = get_conn_state!(global_state)
                .scroll_frames
                .entry(inner.device.clone())
                .or_default();
            let delta = scroll_frame.delta.unwrap_or_default();
            scroll_frame.delta = Some((delta.0 + inner.dx, delta.1 + inner.dy));
        }

        EisRequest::ScrollStop(inner) => {
            let scroll_frame = get_conn_state!(global_state)
                .scroll_frames
                .entry(inner.device.clone())
                .or_default();
            let (stopped, cancelled) = scroll_frame.stop.unwrap_or(((false, false), false));
            scroll_frame.stop = Some(((stopped.0 || inner.x, stopped.1 || inner.y), cancelled));
        }

        EisRequest::ScrollCancel(inner) => {
            let scroll_frame = get_conn_state!(global_state)
                .scroll_frames
                .entry(inner.device.clone())
                .or_default();
            let (stopped, _) = scroll_frame.stop.unwrap_or(((false, false), false));
            scroll_frame.stop = Some(((stopped.0 || inner.x, stopped.1 || inner.y), true));
        }

        EisRequest::ScrollDiscrete(inner) => {
            let scroll_frame = get_conn_state!(global_state)
                .scroll_frames
                .entry(inner.device.clone())
                .or_default();
            let discrete = scroll_frame.discrete.unwrap_or_default();
            scroll_frame.discrete = Some((
                discrete.0 + inner.discrete_dx,
                discrete.1 + inner.discrete_dy,
            ));
        }

        EisRequest::Frame(inner) => {
            let device = inner.device.clone();
            let conn_state = get_conn_state!(global_state);
            let next_frame_touch = conn_state.next_frame_touch.remove(&device);
            let pending_keys = conn_state
                .pending_keyboard
                .remove(&device)
                .unwrap_or_default();
            let scroll_frame = conn_state.scroll_frames.remove(&device);

            for key in coalesce_keyboard_frame(&conn_state.held_keys, pending_keys) {
                let PendingKeyboardKey { event, source } = key;
                let conn_state = get_conn_state!(global_state);
                let held_key = (event.device.clone(), event.key, source);
                if event.state == reis::ei::keyboard::KeyState::Press {
                    conn_state.held_keys.insert(held_key);
                } else {
                    conn_state.held_keys.remove(&held_key);
                }
                conn_state.key_counter = conn_state.held_keys.len().min(u32::MAX as usize) as u32;
                let pressed_count = PressedCount(conn_state.key_counter);
                process_event!(
                    global_state,
                    session_id,
                    event,
                    Keyboard,
                    pressed_count,
                    source
                );
            }

            if let Some(scroll_frame) = scroll_frame {
                let source = get_conn_state!(global_state)
                    .keyboard_source
                    .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
                process_event!(
                    global_state,
                    session_id,
                    inner.clone(),
                    PointerAxis,
                    scroll_frame,
                    source
                )
            }

            if next_frame_touch {
                process_event!(global_state, session_id, inner, TouchFrame, TouchFrame);
            }
        }
        EisRequest::KeyboardKey(inner) => {
            let conn_state = get_conn_state!(global_state);
            let source = conn_state
                .keyboard_source
                .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
            conn_state
                .pending_keyboard
                .entry(inner.device.clone())
                .or_default()
                .push(PendingKeyboardKey {
                    event: inner,
                    source,
                });
        }

        EisRequest::TouchDown(inner) => {
            let conn_state = get_conn_state!(global_state);
            if let Some(regions_extent) = conn_state.regions_extent(&global_state.backend) {
                update_held_input(
                    &mut conn_state.held_touches,
                    inner.device.clone(),
                    inner.touch_id,
                    true,
                );
                let source = conn_state
                    .keyboard_source
                    .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
                conn_state.next_frame_touch.insert(inner.device.clone());
                process_event!(
                    global_state,
                    session_id,
                    inner,
                    TouchDown,
                    AbsolutePositionEventExtra { regions_extent },
                    source
                );
            }
        }
        EisRequest::TouchMotion(inner) => {
            let conn_state = get_conn_state!(global_state);
            if let Some(regions_extent) = conn_state.regions_extent(&global_state.backend) {
                let source = conn_state
                    .keyboard_source
                    .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
                conn_state.next_frame_touch.insert(inner.device.clone());
                process_event!(
                    global_state,
                    session_id,
                    inner,
                    TouchMotion,
                    AbsolutePositionEventExtra { regions_extent },
                    source
                );
            }
        }
        EisRequest::TouchUp(inner) => {
            update_held_input(
                &mut get_conn_state!(global_state).held_touches,
                inner.device.clone(),
                inner.touch_id,
                false,
            );
            let source = get_conn_state!(global_state)
                .keyboard_source
                .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
            get_conn_state!(global_state)
                .next_frame_touch
                .insert(inner.device.clone());
            process_event!(global_state, session_id, inner, TouchUp, (), source);
        }
        EisRequest::TouchCancel(inner) => {
            update_held_input(
                &mut get_conn_state!(global_state).held_touches,
                inner.device.clone(),
                inner.touch_id,
                false,
            );
            let source = get_conn_state!(global_state)
                .keyboard_source
                .unwrap_or(smithay::input::keyboard::KeyboardSource::MAIN);
            get_conn_state!(global_state)
                .next_frame_touch
                .insert(inner.device.clone());
            process_event!(global_state, session_id, inner, TouchCancel, (), source);
        }
    }

    calloop::PostAction::Continue
}

fn update_held_input<T: Eq + std::hash::Hash>(
    held: &mut HashSet<(T, u32)>,
    device: T,
    code: u32,
    pressed: bool,
) {
    if pressed {
        held.insert((device, code));
    } else {
        held.remove(&(device, code));
    }
}

fn coalesce_keyboard_frame(
    held: &HashSet<(
        reis::request::Device,
        u32,
        smithay::input::keyboard::KeyboardSource,
    )>,
    events: Vec<PendingKeyboardKey>,
) -> Vec<PendingKeyboardKey> {
    let keep = coalesced_key_transitions(
        held,
        events.iter().map(|pending| {
            (
                (
                    pending.event.device.clone(),
                    pending.event.key,
                    pending.source,
                ),
                pending.event.state == reis::ei::keyboard::KeyState::Press,
            )
        }),
    );
    events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| keep[index].then_some(event))
        .collect()
}

fn coalesced_key_transitions<K: Eq + std::hash::Hash + Clone>(
    held: &HashSet<K>,
    events: impl Iterator<Item = (K, bool)>,
) -> Vec<bool> {
    let mut final_state = HashMap::new();
    let mut count = 0;
    for (index, (key, pressed)) in events.enumerate() {
        final_state.insert(key.clone(), (index, pressed, held.contains(&key)));
        count += 1;
    }

    let mut keep = vec![false; count];
    for (index, pressed, initially_pressed) in final_state.into_values() {
        keep[index] = pressed != initially_pressed;
    }
    keep
}

fn modifier_keycodes(
    keymap: &xkb::Keymap,
    mod_mask: xkb::ModMask,
    layout_index: u32,
) -> Option<Vec<u32>> {
    let modifiers = [
        (xkb::MOD_NAME_SHIFT, &[Keysym::Shift_L, Keysym::Shift_R][..]),
        (
            xkb::MOD_NAME_CTRL,
            &[Keysym::Control_L, Keysym::Control_R][..],
        ),
        (xkb::MOD_NAME_ALT, &[Keysym::Alt_L, Keysym::Alt_R][..]),
        (xkb::MOD_NAME_LOGO, &[Keysym::Super_L, Keysym::Super_R][..]),
        (
            xkb::MOD_NAME_ISO_LEVEL3_SHIFT,
            &[Keysym::ISO_Level3_Shift][..],
        ),
    ];
    let mut handled_mask = 0;
    let mut keycodes = Vec::new();
    for (name, symbols) in modifiers {
        let index = keymap.mod_get_index(name);
        if index == xkb::MOD_INVALID {
            continue;
        }
        let bit = 1u32 << index;
        if mod_mask & bit == 0 {
            continue;
        }
        handled_mask |= bit;
        let keycode =
            (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find_map(|raw| {
                let keycode = Keycode::new(raw);
                keymap.key_get_name(keycode)?;
                if !keymap
                    .key_get_syms_by_level(keycode, layout_index, 0)
                    .iter()
                    .any(|symbol| symbols.contains(symbol))
                {
                    return None;
                }
                let mut probe = xkb::State::new(keymap);
                probe.update_mask(0, 0, 0, 0, 0, layout_index);
                probe.update_key(keycode, xkb::KeyDirection::Down);
                (probe.serialize_mods(xkb::STATE_MODS_DEPRESSED) & bit != 0)
                    .then_some(raw.saturating_sub(8))
            })?;
        keycodes.push(keycode);
    }
    (mod_mask & !handled_mask == 0).then_some(keycodes)
}

/// Scans the given `keymap` and returns the first keycode (as u32) that produces `keysym` in any
/// level. If none found, returns `None`.
// TODO: Try other groups too, because it's basically trivial to switch groups with
// wl_keyboard.modifiers
fn keysym_to_keycode(
    state: &xkb::State,
    keymap: &xkb::Keymap,
    target_keysym: Keysym,
) -> Option<(Keycode, xkb::ModMask)> {
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    let layout_index = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);

    for keycode in min..=max {
        let keycode = Keycode::new(keycode);

        // Skip unused keycodes
        if keymap.key_get_name(keycode).is_none() {
            continue;
        }

        let num_levels = keymap.num_levels_for_key(keycode, layout_index);
        for level_index in 0..num_levels {
            let syms = keymap.key_get_syms_by_level(keycode, layout_index, level_index);

            if syms != [target_keysym] {
                // Inequal or nonzero count
                continue;
            };

            let mut mod_mask = xkb::ModMask::default();
            let num_masks = keymap.key_get_mods_for_level(
                keycode,
                layout_index,
                level_index,
                std::array::from_mut(&mut mod_mask),
            );

            if num_masks == 0 {
                error!(
                    "Couldn't retrieve modifiers for keycode {} and level {}",
                    keycode.raw(),
                    level_index + 1
                );
                return None;
            }

            return Some((keycode, mod_mask));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Evdev keycode constants from `input-event-codes.h`
    const KEY_SPACE: u32 = 57;
    const KEY_Q: u32 = 16;
    const KEY_A: u32 = 30;

    #[test]
    fn space_to_keycode() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap =
            xkb::Keymap::new_from_names(&ctx, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
                .expect("Failed to compile keymap");
        let state = xkb::State::new(&keymap);

        let keysym = Keysym::space;
        let (keycode, mod_mask) =
            keysym_to_keycode(&state, &keymap, keysym).expect("Could not find keycodepace");

        assert_eq!(keycode.raw(), KEY_SPACE + 8);
        assert_eq!(mod_mask, 0);
    }

    #[test]
    fn keysym_to_keycode_multilayout() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            "us,fr",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("Failed to compile keymap");

        let mut state = xkb::State::new(&keymap);

        // Test the Q key on QWERTY and AZERTY layouts
        let keysym = Keysym::q;

        let (keycode, mod_mask) =
            keysym_to_keycode(&state, &keymap, keysym).expect("Could not find keycode");

        assert_eq!(keycode.raw(), KEY_Q + 8);
        assert_eq!(mod_mask, 0);

        // Wayland clients insert the `group` field of `wl_keyboard.modifiers` into `locked_layout`
        state.update_mask(0, 0, 0, 0, 0, 1);

        let (keycode, mod_mask) =
            keysym_to_keycode(&state, &keymap, keysym).expect("Could not find keycode");

        assert_eq!(keycode.raw(), KEY_A + 8);
        assert_eq!(mod_mask, 0);
    }

    #[test]
    fn modifier_mapping_uses_keymap_remaps_and_rejects_lock_modifiers() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            "us",
            "",
            Some("ctrl:swapcaps".to_string()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("Failed to compile keymap with modifier remap");
        let state = xkb::State::new(&keymap);
        let layout = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let ctrl_index = keymap.mod_get_index(xkb::MOD_NAME_CTRL);
        assert_ne!(ctrl_index, xkb::MOD_INVALID);

        assert_eq!(
            modifier_keycodes(&keymap, 1u32 << ctrl_index, layout),
            Some(vec![58])
        );
        let num_index = keymap.mod_get_index(xkb::MOD_NAME_NUM);
        if num_index != xkb::MOD_INVALID {
            assert_eq!(modifier_keycodes(&keymap, 1u32 << num_index, layout), None);
        }
    }
    #[test]
    fn remote_input_requires_an_active_session() {
        let mut state = RemoteDesktopState::default();
        let session_id = RemoteDesktopSessionId::next();

        assert!(!state.input_is_authorized(session_id));

        state.set_session_active(session_id, true);
        assert!(state.input_is_authorized(session_id));

        state.set_session_active(session_id, false);
        assert!(!state.input_is_authorized(session_id));
    }

    #[test]
    fn touch_cancel_releases_only_active_slots_for_one_device() {
        let mut state = RemoteDesktopState::default();
        let a_active = Some(1).into();
        let a_stale = Some(2).into();
        let b_active = Some(1).into();

        let a_mapped = state.touch_slot("device-a".into(), a_active);
        state.activate_touch_slot("device-a".into(), a_active);
        let a_stale_mapped = state.touch_slot("device-a".into(), a_stale);
        let b_mapped = state.touch_slot("device-b".into(), b_active);
        state.activate_touch_slot("device-b".into(), b_active);

        assert_eq!(state.cancel_touch_slots("device-a"), vec![a_mapped]);
        assert_eq!(state.cancel_touch_slots("device-b"), vec![b_mapped]);
        assert_ne!(state.touch_slot("device-a".into(), a_stale), a_stale_mapped);
    }

    #[test]
    fn touch_cancel_releases_only_the_requested_slot() {
        let mut state = RemoteDesktopState::default();
        let first = Some(1).into();
        let second = Some(2).into();
        let first_mapped = state.touch_slot("device".into(), first);
        let second_mapped = state.touch_slot("device".into(), second);
        state.activate_touch_slot("device".into(), first);
        state.activate_touch_slot("device".into(), second);

        assert_eq!(state.cancel_touch_slot("device", first), Some(first_mapped));
        assert_eq!(state.cancel_touch_slots("device"), vec![second_mapped]);
        assert_ne!(state.touch_slot("device".into(), first), first_mapped);
    }

    #[test]
    fn keyboard_frame_coalesces_net_key_state_without_transient_press() {
        let held = HashSet::from([1]);
        let transitions = coalesced_key_transitions(
            &held,
            [(1, false), (1, true), (2, true), (2, false), (3, true)].into_iter(),
        );
        assert_eq!(transitions, vec![false, false, false, false, true]);
    }

    #[test]
    fn keyboard_frame_keeps_same_key_down_for_distinct_sources() {
        let held = HashSet::new();
        let first_source = smithay::input::keyboard::KeyboardSource::new_auxiliary();
        let second_source = smithay::input::keyboard::KeyboardSource::new_auxiliary();
        let transitions = coalesced_key_transitions(
            &held,
            [
                ((1, 42, first_source), true),
                ((1, 42, second_source), true),
            ]
            .into_iter(),
        );
        assert_eq!(transitions, vec![true, true]);
    }

    #[test]
    fn held_inputs_are_tracked_per_device_and_released_independently() {
        let mut held = HashSet::new();

        update_held_input(&mut held, 1, 30, true);
        update_held_input(&mut held, 2, 30, true);
        update_held_input(&mut held, 1, 30, false);

        assert_eq!(held, HashSet::from([(2, 30)]));
    }
}
