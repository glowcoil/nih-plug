// Clippy doesn't understand it when we use a unit in our `check_null_ptr!()` maccro, even if we
// explicitly pattern match on that unit
#![allow(clippy::unused_unit)]

use atomic_refcell::{AtomicRefCell, AtomicRefMut};
use clap_sys::events::{
    clap_event_header, clap_event_note, clap_event_param_mod, clap_event_param_value,
    clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI,
    CLAP_EVENT_NOTE_EXPRESSION, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_SHOULD_RECORD,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_MONO, CLAP_PORT_STEREO,
};
use clap_sys::ext::audio_ports_config::{
    clap_audio_ports_config, clap_plugin_audio_ports_config, CLAP_EXT_AUDIO_PORTS_CONFIG,
};
use clap_sys::ext::gui::{clap_plugin_gui, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::params::{
    clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_BYPASS,
    CLAP_PARAM_IS_STEPPED,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
    CLAP_PROCESS_ERROR,
};
use clap_sys::stream::{clap_istream, clap_ostream};
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use lazy_static::lazy_static;
use parking_lot::RwLock;
use raw_window_handle::RawWindowHandle;
use std::any::Any;
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_void, CStr};
use std::mem;
use std::os::raw::{c_char, c_ulong};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, ThreadId};

#[cfg(target_os = "macos")]
use clap_sys::ext::gui_cocoa::{clap_plugin_gui_cocoa, CLAP_EXT_GUI_COCOA};
#[cfg(target_os = "windows")]
use clap_sys::ext::gui_win32::{clap_hwnd, clap_plugin_gui_win32, CLAP_EXT_GUI_WIN32};
#[cfg(all(target_family = "unix", not(target_os = "macos")))]
use clap_sys::ext::gui_x11::{clap_plugin_gui_x11, CLAP_EXT_GUI_X11};

use super::context::{WrapperGuiContext, WrapperProcessContext};
use super::descriptor::PluginDescriptor;
use super::util::ClapPtr;
use crate::buffer::Buffer;
use crate::event_loop::{EventLoop, MainThreadExecutor, TASK_QUEUE_CAPACITY};
use crate::param::internals::ParamPtr;
use crate::plugin::{
    BufferConfig, BusConfig, ClapPlugin, Editor, NoteEvent, ParentWindowHandle, ProcessStatus,
};
use crate::wrapper::state;
use crate::wrapper::util::{hash_param_id, process_wrapper, strlcpy};

/// Right now the wrapper adds its own bypass parameter.
///
/// TODO: Actually use this parameter.
pub const BYPASS_PARAM_ID: &str = "bypass";
lazy_static! {
    pub static ref BYPASS_PARAM_HASH: u32 = hash_param_id(BYPASS_PARAM_ID);
}

/// How many output parameter changes we can store in our output parameter change queue. Storing
/// more than this many parmaeters at a time will cause changes to get lost.
const OUTPUT_EVENT_QUEUE_CAPACITY: usize = 2048;

#[repr(C)]
pub struct Wrapper<P: ClapPlugin> {
    // Keep the vtable as the first field so we can do a simple pointer cast
    pub clap_plugin: clap_plugin,

    /// A reference to this object, upgraded to an `Arc<Self>` for the GUI context.
    this: AtomicRefCell<Weak<Self>>,

    /// The wrapped plugin instance.
    plugin: RwLock<P>,
    /// The plugin's editor, if it has one. This object does not do anything on its own, but we need
    /// to instantiate this in advance so we don't need to lock the entire [Plugin] object when
    /// creating an editor.
    editor: Option<Arc<dyn Editor>>,
    /// A handle for the currently active editor instance. The plugin should implement `Drop` on
    /// this handle for its closing behavior.
    editor_handle: RwLock<Option<Box<dyn Any + Send + Sync>>>,

    is_processing: AtomicBool,
    /// The current IO configuration, modified through the `clap_plugin_audio_ports_config`
    /// extension.
    current_bus_config: AtomicCell<BusConfig>,
    /// The current buffer configuration, containing the sample rate and the maximum block size.
    /// Will be set in `clap_plugin::activate()`.
    current_buffer_config: AtomicCell<Option<BufferConfig>>,
    /// Whether the plugin is currently bypassed. This is not yet integrated with the `Plugin`
    /// trait.
    bypass_state: AtomicBool,
    /// The incoming events for the plugin, if `P::ACCEPTS_MIDI` is set.
    ///
    /// TODO: Maybe load these lazily at some point instead of needing to spool them all to this
    ///       queue first
    input_events: AtomicRefCell<VecDeque<NoteEvent>>,
    /// The current latency in samples, as set by the plugin through the [ProcessContext]. uses the
    /// latency extnesion
    pub current_latency: AtomicU32,
    /// Contains slices for the plugin's outputs. You can't directly create a nested slice form
    /// apointer to pointers, so this needs to be preallocated in the setup call and kept around
    /// between process calls. This buffer owns the vector, because otherwise it would need to store
    /// a mutable reference to the data contained in this mutex.
    pub output_buffer: AtomicRefCell<Buffer<'static>>,

    /// Needs to be boxed because the plugin object is supposed to contain a static reference to
    /// this.
    plugin_descriptor: Box<PluginDescriptor<P>>,

    // We'll query all of the host's extensions upfront
    host_callback: ClapPtr<clap_host>,

    clap_plugin_audio_ports_config: clap_plugin_audio_ports_config,
    /// During initialization we'll ask `P` which bus configurations it supports. The host can then
    /// use the audio ports config extension to choose a configuration. Right now we only query mono
    /// and stereo configurations, with and without inputs, as well as the plugin's default input
    /// and output channel counts if that does not match one of those configurations (to do the
    /// least surprising thing).
    ///
    /// TODO: Support surround setups once a plugin needs that
    supported_bus_configs: Vec<BusConfig>,

    clap_plugin_audio_ports: clap_plugin_audio_ports,

    clap_plugin_gui: clap_plugin_gui,

    // These are now platform specific, but after CLAP 0.19 this will all be merged into the regular
    // GUI extension
    #[cfg(all(target_family = "unix", not(target_os = "macos")))]
    clap_plugin_gui_x11: clap_plugin_gui_x11,
    #[cfg(target_os = "macos")]
    clap_plugin_gui_cocoa: clap_plugin_gui_cocoa,
    #[cfg(target_os = "windows")]
    clap_plugin_gui_win32: clap_plugin_gui_win32,

    clap_plugin_latency: clap_plugin_latency,
    host_latency: AtomicRefCell<Option<ClapPtr<clap_host_latency>>>,

    clap_plugin_params: clap_plugin_params,
    host_params: AtomicRefCell<Option<ClapPtr<clap_host_params>>>,
    // These fiels are exactly the same as their VST3 wrapper counterparts.
    //
    /// The keys from `param_map` in a stable order.
    param_hashes: Vec<u32>,
    /// A mapping from parameter ID hashes (obtained from the string parameter IDs) to pointers to
    /// parameters belonging to the plugin. As long as `plugin` does not get recreated, these
    /// addresses will remain stable, as they are obtained from a pinned object.
    param_by_hash: HashMap<u32, ParamPtr>,
    /// The default normalized parameter value for every parameter in `param_ids`. We need to store
    /// this in case the host requeries the parmaeter later. This is also indexed by the hash so we
    /// can retrieve them later for the UI if needed.
    pub param_defaults_normalized: HashMap<u32, f32>,
    /// Mappings from string parameter indentifiers to parameter hashes. Useful for debug logging
    /// and when storing and restoring plugin state.
    param_id_to_hash: HashMap<&'static str, u32>,
    /// The inverse mapping from [Self::param_by_hash]. This is needed to be able to have an
    /// ergonomic parameter setting API that uses references to the parameters instead of having to
    /// add a setter function to the parameter (or even worse, have it be completely untyped).
    pub param_ptr_to_hash: HashMap<ParamPtr, u32>,
    /// A queue of parameter changes that should be output in either the next process call or in the
    /// next parameter flush.
    ///
    /// XXX: There's no guarentee that a single parmaeter doesn't occur twice in this queue, but
    ///      even if it does then that should still not be a problem because the host also reads it
    ///      in the same order, right?
    output_parameter_changes: ArrayQueue<OutputParamChange>,

    host_thread_check: AtomicRefCell<Option<ClapPtr<clap_host_thread_check>>>,

    clap_plugin_state: clap_plugin_state,

    /// A queue of tasks that still need to be performed. Because CLAP lets the plugin request a
    /// host callback directly, we don't need to use the OsEventLoop we use in our other plugin
    /// implementations. Instead, we'll post tasks to this queue, ask the host to call
    /// [Self::on_main_thread] on the main thread, and then continue to pop tasks off this queue
    /// there until it is empty.
    tasks: ArrayQueue<Task>,
    /// The ID of the main thread. In practice this is the ID of the thread that created this
    /// object. If the host supports the thread check extension (and [Self::thread_check] thus
    /// contains a value), then that extension is used instead.
    main_thread_id: ThreadId,
}

/// Tasks that can be sent from the plugin to be executed on the main thread in a non-blocking
/// realtime safe way. Instead of using a random thread or the OS' event loop like in the Linux
/// implementation, this uses [clap_host::request_callback()] instead.
#[derive(Debug, Clone)]
pub enum Task {
    /// Inform the host that the latency has changed.
    LatencyChanged,
}

/// The types of CLAP parameter updates for events.
pub enum ClapParamUpdate {
    /// Set the parameter to this plain value. In our wrapper the plain values are the normalized
    /// values multiplied by the step count for discrete parameters.
    PlainValueSet(f64),
    /// Add a delta to the parameter's current plain value (so again, multiplied by the step size).
    PlainValueMod(f64),
}

/// A parameter change that should be output by the plugin, stored in a queue on the wrapper and
/// written to the host either at the end of the process function or during a flush.
pub struct OutputParamChange {
    /// The internal hash for the parameter.
    pub param_hash: u32,
    /// The 'plain' value as reported to CLAP. This is the normalized value multiplied by
    /// [crate::Param::step_size].
    pub clap_plain_value: f64,
}

/// Because CLAP has this [clap_host::request_host_callback()] function, we don't need to use
/// `OsEventLoop` and can instead just request a main thread callback directly.
impl<P: ClapPlugin> EventLoop<Task, Wrapper<P>> for Wrapper<P> {
    fn new_and_spawn(_executor: std::sync::Weak<Self>) -> Self {
        panic!("What are you doing");
    }

    fn do_maybe_async(&self, task: Task) -> bool {
        if self.is_main_thread() {
            unsafe { self.execute(task) };
            true
        } else {
            let success = self.tasks.push(task).is_ok();
            if success {
                // CLAP lets us use the host's event loop instead of having to implement our own
                let host = &self.host_callback;
                unsafe { (host.request_callback)(&**host) };
            }

            success
        }
    }

    fn is_main_thread(&self) -> bool {
        // If the host supports the thread check interface then we'll use that, otherwise we'll
        // check if this is the same thread as the one that created the plugin instance.
        match &*self.host_thread_check.borrow() {
            Some(thread_check) => unsafe { (thread_check.is_main_thread)(&*self.host_callback) },
            None => thread::current().id() == self.main_thread_id,
        }
    }
}

impl<P: ClapPlugin> MainThreadExecutor<Task> for Wrapper<P> {
    unsafe fn execute(&self, task: Task) {
        // This function is always called from the main thread, from [Self::on_main_thread].
        match task {
            Task::LatencyChanged => match &*self.host_latency.borrow() {
                Some(host_latency) => (host_latency.changed)(&*self.host_callback),
                None => nih_debug_assert_failure!("Host does not support the latency extension"),
            },
        };
    }
}

impl<P: ClapPlugin> Wrapper<P> {
    pub fn new(host_callback: *const clap_host) -> Arc<Self> {
        let plugin = RwLock::new(P::default());
        let editor = plugin.read().editor().map(Arc::from);

        let plugin_descriptor = Box::new(PluginDescriptor::default());

        // We're not allowed to query any extensions until the init function has been called, so we
        // need a bunch of AtomicRefCells instead
        assert!(!host_callback.is_null());
        let host_callback = unsafe { ClapPtr::new(host_callback) };

        let mut wrapper = Self {
            clap_plugin: clap_plugin {
                // This needs to live on the heap because the plugin object contains a direct
                // reference to the manifest as a value. We could share this between instances of
                // the plugin using an `Arc`, but this doesn't consume a lot of memory so it's not a
                // huge deal.
                desc: plugin_descriptor.clap_plugin_descriptor(),
                // We already need to use pointer casts in the factory, so might as well continue
                // doing that here
                plugin_data: ptr::null_mut(),
                init: Self::init,
                destroy: Self::destroy,
                activate: Self::activate,
                deactivate: Self::deactivate,
                start_processing: Self::start_processing,
                stop_processing: Self::stop_processing,
                process: Self::process,
                get_extension: Self::get_extension,
                on_main_thread: Self::on_main_thread,
            },

            this: AtomicRefCell::new(Weak::new()),

            plugin,
            editor,
            editor_handle: RwLock::new(None),

            is_processing: AtomicBool::new(false),
            current_bus_config: AtomicCell::new(BusConfig {
                num_input_channels: P::DEFAULT_NUM_INPUTS,
                num_output_channels: P::DEFAULT_NUM_OUTPUTS,
            }),
            current_buffer_config: AtomicCell::new(None),
            bypass_state: AtomicBool::new(false),
            input_events: AtomicRefCell::new(VecDeque::with_capacity(512)),
            current_latency: AtomicU32::new(0),
            output_buffer: AtomicRefCell::new(Buffer::default()),

            plugin_descriptor,

            host_callback,

            clap_plugin_audio_ports_config: clap_plugin_audio_ports_config {
                count: Self::ext_audio_ports_config_count,
                get: Self::ext_audio_ports_config_get,
                select: Self::ext_audio_ports_config_select,
            },
            supported_bus_configs: Vec::new(),

            clap_plugin_audio_ports: clap_plugin_audio_ports {
                count: Self::ext_audio_ports_count,
                get: Self::ext_audio_ports_get,
            },

            clap_plugin_gui: clap_plugin_gui {
                create: Self::ext_gui_create,
                destroy: Self::ext_gui_destroy,
                set_scale: Self::ext_gui_set_scale,
                get_size: Self::ext_gui_get_size,
                can_resize: Self::ext_gui_can_resize,
                round_size: Self::ext_gui_round_size,
                set_size: Self::ext_gui_set_size,
                show: Self::ext_gui_show,
                hide: Self::ext_gui_hide,
            },

            #[cfg(all(target_family = "unix", not(target_os = "macos")))]
            clap_plugin_gui_x11: clap_plugin_gui_x11 {
                attach: Self::ext_gui_x11_attach,
            },
            #[cfg(target_os = "macos")]
            clap_plugin_gui_cocoa: clap_plugin_gui_cocoa {
                attach: Self::ext_gui_cocoa_attach,
            },
            #[cfg(target_os = "windows")]
            clap_plugin_gui_win32: clap_plugin_gui_win32 {
                attach: Self::ext_gui_win32_attach,
            },

            clap_plugin_latency: clap_plugin_latency {
                get: Self::ext_latency_get,
            },
            host_latency: AtomicRefCell::new(None),

            clap_plugin_params: clap_plugin_params {
                count: Self::ext_params_count,
                get_info: Self::ext_params_get_info,
                get_value: Self::ext_params_get_value,
                value_to_text: Self::ext_params_value_to_text,
                text_to_value: Self::ext_params_text_to_value,
                flush: Self::ext_params_flush,
            },
            host_params: AtomicRefCell::new(None),
            param_hashes: Vec::new(),
            param_by_hash: HashMap::new(),
            param_defaults_normalized: HashMap::new(),
            param_id_to_hash: HashMap::new(),
            param_ptr_to_hash: HashMap::new(),
            output_parameter_changes: ArrayQueue::new(OUTPUT_EVENT_QUEUE_CAPACITY),

            host_thread_check: AtomicRefCell::new(None),

            clap_plugin_state: clap_plugin_state {
                save: Self::ext_state_save,
                load: Self::ext_state_load,
            },

            tasks: ArrayQueue::new(TASK_QUEUE_CAPACITY),
            main_thread_id: thread::current().id(),
        };

        // Query all sensible bus configurations supported by the plugin. We don't do surround or
        // anything beyond stereo right now.
        for num_output_channels in [1, 2] {
            for num_input_channels in [0, num_output_channels] {
                let bus_config = BusConfig {
                    num_input_channels,
                    num_output_channels,
                };
                if wrapper.plugin.read().accepts_bus_config(&bus_config) {
                    wrapper.supported_bus_configs.push(bus_config);
                }
            }
        }

        // In the off chance that the default config specified by the plugin is not in the above
        // list, we'll try that as well.
        let default_bus_config = BusConfig {
            num_input_channels: P::DEFAULT_NUM_INPUTS,
            num_output_channels: P::DEFAULT_NUM_OUTPUTS,
        };
        if !wrapper.supported_bus_configs.contains(&default_bus_config)
            && wrapper
                .plugin
                .read()
                .accepts_bus_config(&default_bus_config)
        {
            wrapper.supported_bus_configs.push(default_bus_config);
        }

        // This is a mapping from the parameter IDs specified by the plugin to pointers to thsoe
        // parameters. Since the object returned by `params()` is pinned, these pointers are safe to
        // dereference as long as `wrapper.plugin` is alive
        let param_map = wrapper.plugin.read().params().param_map();
        let param_ids = wrapper.plugin.read().params().param_ids();
        nih_debug_assert!(
            !param_map.contains_key(BYPASS_PARAM_ID),
            "The wrapper already adds its own bypass parameter"
        );

        // Only calculate these hashes once, and in the stable order defined by the plugin
        let param_id_hashes_ptrs: Vec<_> = param_ids
            .iter()
            .filter_map(|id| {
                let param_ptr = param_map.get(id)?;
                Some((id, hash_param_id(id), param_ptr))
            })
            .collect();
        wrapper.param_hashes = param_id_hashes_ptrs
            .iter()
            .map(|&(_, hash, _)| hash)
            .collect();
        wrapper.param_by_hash = param_id_hashes_ptrs
            .iter()
            .map(|&(_, hash, ptr)| (hash, *ptr))
            .collect();
        wrapper.param_defaults_normalized = param_id_hashes_ptrs
            .iter()
            .map(|&(_, hash, ptr)| (hash, unsafe { ptr.normalized_value() }))
            .collect();
        wrapper.param_id_to_hash = param_id_hashes_ptrs
            .iter()
            .map(|&(id, hash, _)| (*id, hash))
            .collect();
        wrapper.param_ptr_to_hash = param_id_hashes_ptrs
            .into_iter()
            .map(|(_, hash, ptr)| (*ptr, hash))
            .collect();

        // Finally, the wrapper needs to contain a reference to itself so we can create GuiContexts
        // when opening plugin editors
        let wrapper = Arc::new(wrapper);
        *wrapper.this.borrow_mut() = Arc::downgrade(&wrapper);

        wrapper
    }

    fn make_gui_context(self: Arc<Self>) -> Arc<WrapperGuiContext<P>> {
        Arc::new(WrapperGuiContext { wrapper: self })
    }

    fn make_process_context(&self) -> WrapperProcessContext<'_, P> {
        WrapperProcessContext {
            wrapper: self,
            input_events_guard: self.input_events.borrow_mut(),
        }
    }

    /// Queue a parmeter change to be sent to the host at the end of the audio processing cycle, and
    /// request a parameter flush from the host if the plugin is not currently processing audio. The
    /// parameter's actual value will only be updated at that point so the value won't change in the
    /// middle of a processing call.
    ///
    /// Returns `false` if the parameter value queue was full and the update will not be sent to the
    /// host (it will still be set on the plugin either way).
    pub fn queue_parameter_change(&self, change: OutputParamChange) -> bool {
        let result = self.output_parameter_changes.push(change).is_ok();
        match &*self.host_params.borrow() {
            Some(host_params) if !self.is_processing.load(Ordering::SeqCst) => {
                unsafe { (host_params.request_flush)(&*self.host_callback) };
            }
            _ => nih_debug_assert_failure!("The host does not support parameters? What?"),
        }

        result
    }

    /// Convenience function for setting a value for a parameter as triggered by a VST3 parameter
    /// update. The same rate is for updating parameter smoothing.
    ///
    /// # Note
    ///
    /// These values are CLAP plain values, which include a step count multiplier for discrete
    /// parameter values.
    pub fn update_plain_value_by_hash(
        &self,
        hash: u32,
        update: ClapParamUpdate,
        sample_rate: Option<f32>,
    ) -> bool {
        if hash == *BYPASS_PARAM_HASH {
            match update {
                ClapParamUpdate::PlainValueSet(clap_plain_value) => self
                    .bypass_state
                    .store(clap_plain_value >= 0.5, Ordering::SeqCst),
                ClapParamUpdate::PlainValueMod(clap_plain_mod) => {
                    if clap_plain_mod > 0.0 {
                        self.bypass_state.store(true, Ordering::SeqCst)
                    } else if clap_plain_mod < 0.0 {
                        self.bypass_state.store(false, Ordering::SeqCst)
                    }
                }
            }

            true
        } else if let Some(param_ptr) = self.param_by_hash.get(&hash) {
            let normalized_value = match update {
                ClapParamUpdate::PlainValueSet(clap_plain_value) => {
                    clap_plain_value as f32 / unsafe { param_ptr.step_count() }.unwrap_or(1) as f32
                }
                ClapParamUpdate::PlainValueMod(clap_plain_mod) => {
                    let current_normalized_value = unsafe { param_ptr.normalized_value() };
                    current_normalized_value
                        + (clap_plain_mod as f32
                            / unsafe { param_ptr.step_count() }.unwrap_or(1) as f32)
                }
            };

            // Also update the parameter's smoothing if applicable
            match (param_ptr, sample_rate) {
                (_, Some(sample_rate)) => unsafe {
                    param_ptr.set_normalized_value(normalized_value);
                    param_ptr.update_smoother(sample_rate, false);
                },
                _ => unsafe { param_ptr.set_normalized_value(normalized_value) },
            }

            true
        } else {
            false
        }
    }

    /// Handle all incoming events from an event queue. This will clearn `self.input_events` first.
    pub unsafe fn handle_in_events(&self, in_: &clap_input_events) {
        let mut input_events = self.input_events.borrow_mut();
        input_events.clear();

        let num_events = ((*in_).size)(&*in_);
        for event_idx in 0..num_events {
            let event = ((*in_).get)(&*in_, event_idx);
            self.handle_event(event, &mut input_events);
        }
    }

    /// Write the unflushed parameter changes to the host's output event queue. This will also
    /// modify the actual parameter values, since we should only do that while the wrapped plugin is
    /// not actually processing audio.
    pub unsafe fn handle_out_events(&self, out: &clap_output_events) {
        // We'll always write these events to the first sample, so even when we add note output we
        // shouldn't have to think about interleaving events here
        let sample_rate = self.current_buffer_config.load().map(|c| c.sample_rate);
        while let Some(change) = self.output_parameter_changes.pop() {
            self.update_plain_value_by_hash(
                change.param_hash,
                ClapParamUpdate::PlainValueSet(change.clap_plain_value),
                sample_rate,
            );

            let event = clap_event_param_value {
                header: clap_event_header {
                    size: mem::size_of::<clap_event_param_value>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: CLAP_EVENT_SHOULD_RECORD,
                },
                param_id: change.param_hash,
                cookie: ptr::null_mut(),
                port_index: -1,
                key: -1,
                channel: -1,
                value: change.clap_plain_value,
            };

            (out.push_back)(out, &event.header);
        }
    }

    /// Handle an incoming CLAP event. You must clear [Self::input_events] first before calling this
    /// from the process function.
    ///
    /// To save on mutex operations when handing MIDI events, the lock guard for the input events
    /// need to be passed into this function.
    pub unsafe fn handle_event(
        &self,
        event: *const clap_event_header,
        input_events: &mut AtomicRefMut<VecDeque<NoteEvent>>,
    ) {
        let raw_event = &*event;
        match (raw_event.space_id, raw_event.type_) {
            // TODO: Implement the event filter
            // TODO: Handle sample accurate parameter changes, possibly in a similar way to the
            //       smoothing
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE) => {
                let event = &*(event as *const clap_event_param_value);
                self.update_plain_value_by_hash(
                    event.param_id,
                    ClapParamUpdate::PlainValueSet(event.value),
                    self.current_buffer_config.load().map(|c| c.sample_rate),
                );
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_MOD) => {
                let event = &*(event as *const clap_event_param_mod);
                self.update_plain_value_by_hash(
                    event.param_id,
                    ClapParamUpdate::PlainValueMod(event.amount),
                    self.current_buffer_config.load().map(|c| c.sample_rate),
                );
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_ON) => {
                if P::ACCEPTS_MIDI {
                    let event = &*(event as *const clap_event_note);
                    input_events.push_back(NoteEvent::NoteOn {
                        timing: raw_event.time,
                        channel: event.channel as u8,
                        note: event.key as u8,
                        velocity: (event.velocity * 127.0).round() as u8,
                    });
                }
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF) => {
                if P::ACCEPTS_MIDI {
                    let event = &*(event as *const clap_event_note);
                    input_events.push_back(NoteEvent::NoteOff {
                        timing: raw_event.time,
                        channel: event.channel as u8,
                        note: event.key as u8,
                        velocity: (event.velocity * 127.0).round() as u8,
                    });
                }
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_EXPRESSION) => {
                if P::ACCEPTS_MIDI {
                    // TODO: Implement pressure and other expressions along with MIDI CCs
                }
            }
            (CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI) => {
                if P::ACCEPTS_MIDI {
                    // TODO: Implement raw MIDI handling once we add CCs
                }
            }
            // TODO: Make sure this only gets logged in debug mode
            _ => nih_log!(
                "Unhandled CLAP event type {} for namespace {}",
                raw_event.type_,
                raw_event.space_id
            ),
        }
    }

    unsafe extern "C" fn init(plugin: *const clap_plugin) -> bool {
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        // We weren't allowed to query these in the constructor, so we need to do it now intead.
        *wrapper.host_latency.borrow_mut() =
            query_host_extension::<clap_host_latency>(&wrapper.host_callback, CLAP_EXT_LATENCY);
        *wrapper.host_params.borrow_mut() =
            query_host_extension::<clap_host_params>(&wrapper.host_callback, CLAP_EXT_PARAMS);
        *wrapper.host_thread_check.borrow_mut() = query_host_extension::<clap_host_thread_check>(
            &wrapper.host_callback,
            CLAP_EXT_THREAD_CHECK,
        );

        true
    }

    unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
        Arc::from_raw(plugin as *mut Self);
    }

    unsafe extern "C" fn activate(
        plugin: *const clap_plugin,
        sample_rate: f64,
        _min_frames_count: u32,
        max_frames_count: u32,
    ) -> bool {
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        let bus_config = wrapper.current_bus_config.load();
        let buffer_config = BufferConfig {
            sample_rate: sample_rate as f32,
            max_buffer_size: max_frames_count,
        };

        // Befure initializing the plugin, make sure all smoothers are set the the default values
        for param in wrapper.param_by_hash.values() {
            param.update_smoother(buffer_config.sample_rate, true);
        }

        if wrapper.plugin.write().initialize(
            &bus_config,
            &buffer_config,
            &mut wrapper.make_process_context(),
        ) {
            // Preallocate enough room in the output slices vector so we can convert a `*mut *mut
            // f32` to a `&mut [&mut f32]` in the process call
            wrapper
                .output_buffer
                .borrow_mut()
                .with_raw_vec(|output_slices| {
                    output_slices.resize_with(bus_config.num_output_channels as usize, || &mut [])
                });

            // Also store this for later, so we can reinitialize the plugin after restoring state
            wrapper.current_buffer_config.store(Some(buffer_config));

            true
        } else {
            false
        }
    }

    unsafe extern "C" fn deactivate(_plugin: *const clap_plugin) {
        // We currently don't do anything here
    }

    unsafe extern "C" fn start_processing(plugin: *const clap_plugin) -> bool {
        // We just need to keep track of our processing state so we can request a flush when
        // updating parameters from the GUI while the processing loop isn't running
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        wrapper.is_processing.store(true, Ordering::SeqCst);

        true
    }

    unsafe extern "C" fn stop_processing(plugin: *const clap_plugin) {
        check_null_ptr!((), plugin);
        let wrapper = &*(plugin as *const Self);

        wrapper.is_processing.store(false, Ordering::SeqCst);
    }

    unsafe extern "C" fn process(
        plugin: *const clap_plugin,
        process: *const clap_process,
    ) -> clap_process_status {
        check_null_ptr!(CLAP_PROCESS_ERROR, plugin, process);
        let wrapper = &*(plugin as *const Self);

        // Panic on allocations if the `assert_process_allocs` feature has been enabled, and make
        // sure that FTZ is set up correctly
        process_wrapper(|| {
            // We need to handle incoming automation and MIDI events. Since we don't support sample
            // accuration automation yet and there's no way to get the last event for a parameter,
            // we'll process every incoming event.
            let process = &*process;
            if !process.in_events.is_null() {
                wrapper.handle_in_events(&*process.in_events);
            }

            // I don't think this is a thing for CLAP since there's a dedicated flush function, but
            // might as well protect against this
            // TOOD: Send the output events when doing a flush
            if process.audio_outputs_count == 0 || process.frames_count == 0 {
                nih_log!("CLAP process call event flush");
                return CLAP_PROCESS_CONTINUE;
            }

            // The setups we suppport are:
            // - 1 input bus
            // - 1 output bus
            // - 1 input bus and 1 output bus
            nih_debug_assert!(
                process.audio_inputs_count <= 1 && process.audio_outputs_count <= 1,
                "The host provides more than one input or output bus"
            );

            // Right now we don't handle any auxiliary outputs
            check_null_ptr_msg!(
                "Null pointers passed for audio outputs in process function",
                CLAP_PROCESS_ERROR,
                process.audio_outputs,
                (*process.audio_outputs).data32
            );
            let audio_outputs = &*process.audio_outputs;
            let num_output_channels = audio_outputs.channel_count as usize;

            // This vector has been preallocated to contain enough slices as there are output
            // channels
            // TODO: The audio buffers have a latency field, should we use those?
            // TODO: Like with VST3, should we expose some way to access or set the silence/constant
            //       flags?
            let mut output_buffer = wrapper.output_buffer.borrow_mut();
            output_buffer.with_raw_vec(|output_slices| {
                nih_debug_assert_eq!(num_output_channels, output_slices.len());
                for (output_channel_idx, output_channel_slice) in
                    output_slices.iter_mut().enumerate()
                {
                    // SAFETY: These pointers may not be valid outside of this function even though
                    // their lifetime is equal to this structs. This is still safe because they are
                    // only dereferenced here later as part of this process function.
                    *output_channel_slice = std::slice::from_raw_parts_mut(
                        *(audio_outputs.data32 as *mut *mut f32).add(output_channel_idx),
                        process.frames_count as usize,
                    );
                }
            });

            // Most hosts process data in place, in which case we don't need to do any copying
            // ourselves. If the pointers do not alias, then we'll do the copy here and then the
            // plugin can just do normal in place processing.
            if !process.audio_inputs.is_null() {
                // We currently don't support sidechain inputs
                let audio_inputs = &*process.audio_inputs;
                let num_input_channels = audio_inputs.channel_count as usize;
                nih_debug_assert!(
                    num_input_channels <= num_output_channels,
                    "Stereo to mono and similar configurations are not supported"
                );
                for input_channel_idx in 0..cmp::min(num_input_channels, num_output_channels) {
                    let output_channel_ptr =
                        *(audio_outputs.data32 as *mut *mut f32).add(input_channel_idx);
                    let input_channel_ptr = *(audio_inputs.data32).add(input_channel_idx);
                    if input_channel_ptr != output_channel_ptr {
                        ptr::copy_nonoverlapping(
                            input_channel_ptr,
                            output_channel_ptr,
                            process.frames_count as usize,
                        );
                    }
                }
            }

            let mut plugin = wrapper.plugin.write();
            let mut context = wrapper.make_process_context();
            let result = match plugin.process(&mut output_buffer, &mut context) {
                ProcessStatus::Error(err) => {
                    nih_debug_assert_failure!("Process error: {}", err);

                    CLAP_PROCESS_ERROR
                }
                ProcessStatus::Normal => CLAP_PROCESS_CONTINUE_IF_NOT_QUIET,
                ProcessStatus::Tail(_) => CLAP_PROCESS_CONTINUE,
                ProcessStatus::KeepAlive => CLAP_PROCESS_CONTINUE,
            };

            // After processing audio, send all spooled events to the host
            if !process.out_events.is_null() {
                wrapper.handle_out_events(&*process.out_events);
            }

            result
        })
    }

    unsafe extern "C" fn get_extension(
        plugin: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        check_null_ptr!(ptr::null(), plugin, id);
        let wrapper = &*(plugin as *const Self);

        let id = CStr::from_ptr(id);

        // These extensions are only relevant on the respective platforms and they will be removed
        // entirely in CLAP 0.19
        #[cfg(all(target_family = "unix", not(target_os = "macos")))]
        if id == CStr::from_ptr(CLAP_EXT_GUI_X11) && wrapper.editor.is_some() {
            return &wrapper.clap_plugin_gui_x11 as *const _ as *const c_void;
        }
        #[cfg(target_os = "macos")]
        if id == CStr::from_ptr(CLAP_EXT_GUI_COCOA) && wrapper.editor.is_some() {
            return &wrapper.clap_plugin_gui_cocoa as *const _ as *const c_void;
        }
        #[cfg(target_os = "windows")]
        if id == CStr::from_ptr(CLAP_EXT_GUI_WIN32) && wrapper.editor.is_some() {
            return &wrapper.clap_plugin_gui_win32 as *const _ as *const c_void;
        }

        if id == CStr::from_ptr(CLAP_EXT_AUDIO_PORTS_CONFIG) {
            &wrapper.clap_plugin_audio_ports_config as *const _ as *const c_void
        } else if id == CStr::from_ptr(CLAP_EXT_AUDIO_PORTS) {
            &wrapper.clap_plugin_audio_ports as *const _ as *const c_void
        } else if id == CStr::from_ptr(CLAP_EXT_GUI) && wrapper.editor.is_some() {
            // Only report that we support this extension if the plugin has an editor
            &wrapper.clap_plugin_gui as *const _ as *const c_void
        } else if id == CStr::from_ptr(CLAP_EXT_LATENCY) {
            &wrapper.clap_plugin_latency as *const _ as *const c_void
        } else if id == CStr::from_ptr(CLAP_EXT_PARAMS) {
            &wrapper.clap_plugin_params as *const _ as *const c_void
        } else if id == CStr::from_ptr(CLAP_EXT_STATE) {
            &wrapper.clap_plugin_state as *const _ as *const c_void
        } else {
            nih_log!("Host tried to query unknown extension {:?}", id);
            ptr::null()
        }
    }

    unsafe extern "C" fn on_main_thread(plugin: *const clap_plugin) {
        check_null_ptr!((), plugin);
        let wrapper = &*(plugin as *const Self);

        // [Self::do_maybe_async] posts a task to the queue and asks the host to call this function
        // on the main thread, so once that's done we can just handle all requests here
        while let Some(task) = wrapper.tasks.pop() {
            wrapper.execute(task);
        }
    }

    unsafe extern "C" fn ext_audio_ports_config_count(plugin: *const clap_plugin) -> u32 {
        check_null_ptr!(0, plugin);
        let wrapper = &*(plugin as *const Self);

        wrapper.supported_bus_configs.len() as u32
    }

    unsafe extern "C" fn ext_audio_ports_config_get(
        plugin: *const clap_plugin,
        index: u32,
        config: *mut clap_audio_ports_config,
    ) -> bool {
        check_null_ptr!(false, plugin, config);
        let wrapper = &*(plugin as *const Self);

        match wrapper.supported_bus_configs.get(index as usize) {
            Some(bus_config) => {
                let name = match bus_config {
                    BusConfig {
                        num_input_channels: _,
                        num_output_channels: 1,
                    } => String::from("Mono"),
                    BusConfig {
                        num_input_channels: _,
                        num_output_channels: 2,
                    } => String::from("Stereo"),
                    BusConfig {
                        num_input_channels,
                        num_output_channels,
                    } => format!("{num_input_channels} inputs, {num_output_channels} outputs"),
                };
                let input_port_type = match bus_config.num_input_channels {
                    1 => CLAP_PORT_MONO,
                    2 => CLAP_PORT_STEREO,
                    _ => ptr::null(),
                };
                let output_port_type = match bus_config.num_output_channels {
                    1 => CLAP_PORT_MONO,
                    2 => CLAP_PORT_STEREO,
                    _ => ptr::null(),
                };

                *config = std::mem::zeroed();

                let config = &mut *config;
                config.id = index;
                strlcpy(&mut config.name, &name);
                config.input_channel_count = bus_config.num_input_channels;
                config.input_port_type = input_port_type;
                config.output_channel_count = bus_config.num_output_channels;
                config.output_port_type = output_port_type;

                true
            }
            None => {
                nih_debug_assert_failure!(
                    "Host tried to query out of bounds audio port config {}",
                    index
                );

                false
            }
        }
    }

    unsafe extern "C" fn ext_audio_ports_config_select(
        plugin: *const clap_plugin,
        config_id: clap_id,
    ) -> bool {
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        // We use the vector indices for the config ID
        match wrapper.supported_bus_configs.get(config_id as usize) {
            Some(bus_config) => {
                wrapper.current_bus_config.store(*bus_config);

                true
            }
            None => {
                nih_debug_assert_failure!(
                    "Host tried to select out of bounds audio port config {}",
                    config_id
                );

                false
            }
        }
    }

    unsafe extern "C" fn ext_audio_ports_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
        // TODO: Implement sidechain nputs and auxiliary outputs
        check_null_ptr!(0, plugin);
        let wrapper = &*(plugin as *const Self);

        let bus_config = wrapper.current_bus_config.load();
        match (
            is_input,
            bus_config.num_input_channels,
            bus_config.num_output_channels,
        ) {
            (true, 0, _) => 0,
            // This should not be possible, however
            (false, _, 0) => 0,
            _ => 1,
        }
    }

    unsafe extern "C" fn ext_audio_ports_get(
        plugin: *const clap_plugin,
        index: u32,
        is_input: bool,
        info: *mut clap_audio_port_info,
    ) -> bool {
        check_null_ptr!(false, plugin, info);
        let wrapper = &*(plugin as *const Self);

        const INPUT_ID: u32 = 0;
        const OUTPUT_ID: u32 = 1;

        // Even if we don't report having ports when the number of channels are 0, might as well
        // handle them here anyways in case we do need to always report them in the future
        match index {
            0 => {
                let current_bus_config = wrapper.current_bus_config.load();
                let channel_count = if is_input {
                    current_bus_config.num_input_channels
                } else {
                    current_bus_config.num_output_channels
                };

                // When we add sidechain inputs and auxiliary outputs this would need some changing
                let stable_id = if is_input { INPUT_ID } else { OUTPUT_ID };
                let pair_stable_id = if is_input && current_bus_config.num_output_channels > 0 {
                    OUTPUT_ID
                } else if !is_input && current_bus_config.num_input_channels > 0 {
                    INPUT_ID
                } else {
                    CLAP_INVALID_ID
                };
                let port_type_name = if is_input { "Input" } else { "Output" };
                let name = match channel_count {
                    1 => format!("Mono {port_type_name}"),
                    2 => format!("Stereo {port_type_name}"),
                    n => format!("{n} channel {port_type_name}"),
                };
                let port_type = match channel_count {
                    1 => CLAP_PORT_MONO,
                    2 => CLAP_PORT_STEREO,
                    _ => ptr::null(),
                };

                *info = std::mem::zeroed();

                let info = &mut *info;
                info.id = stable_id;
                strlcpy(&mut info.name, &name);
                info.flags = CLAP_AUDIO_PORT_IS_MAIN;
                info.channel_count = channel_count;
                info.port_type = port_type;
                info.in_place_pair = pair_stable_id;

                true
            }
            _ => {
                nih_debug_assert_failure!(
                    "Host tried to query information for out of bounds audio port {} (input: {})",
                    index,
                    is_input
                );

                false
            }
        }
    }

    unsafe extern "C" fn ext_gui_create(plugin: *const clap_plugin) -> bool {
        // This is a bit weird, because in CLAP 0.18 creating the editor window and embedding it in
        // another window are separate, and those things are one and the same in our framework. So
        // we'll just pretend we did something here.
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        let editor_handle = wrapper.editor_handle.read();
        if editor_handle.is_none() {
            true
        } else {
            nih_debug_assert_failure!("Tried creating editor while the editor was already active");
            false
        }
    }

    unsafe extern "C" fn ext_gui_destroy(plugin: *const clap_plugin) {
        check_null_ptr!((), plugin);
        let wrapper = &*(plugin as *const Self);

        let mut editor_handle = wrapper.editor_handle.write();
        if editor_handle.is_some() {
            *editor_handle = None;
        } else {
            nih_debug_assert_failure!("Tried destroying editor while the editor was not active");
        }
    }

    unsafe extern "C" fn ext_gui_set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
        // TOOD: Implement DPI scaling
        false
    }

    unsafe extern "C" fn ext_gui_get_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        check_null_ptr!(false, plugin, width, height);
        let wrapper = &*(plugin as *const Self);

        match &wrapper.editor {
            Some(editor) => {
                (*width, *height) = editor.size();
                true
            }
            None => {
                unreachable!("We don't return the editor extension on plugins without an editor");
            }
        }
    }

    unsafe extern "C" fn ext_gui_can_resize(_plugin: *const clap_plugin) -> bool {
        // TODO: Implement GUI resizing
        false
    }

    unsafe extern "C" fn ext_gui_round_size(
        _plugin: *const clap_plugin,
        _width: *mut u32,
        _height: *mut u32,
    ) {
        // TODO: Implement GUI resizing
    }

    unsafe extern "C" fn ext_gui_set_size(
        plugin: *const clap_plugin,
        width: u32,
        height: u32,
    ) -> bool {
        // TODO: Implement GUI resizing
        check_null_ptr!(false, plugin);
        let wrapper = &*(plugin as *const Self);

        match &wrapper.editor {
            Some(editor) => {
                let (editor_width, editor_height) = editor.size();
                width == editor_width && height == editor_height
            }
            None => {
                unreachable!("We don't return the editor extension on plugins without an editor");
            }
        }
    }

    unsafe extern "C" fn ext_gui_show(_plugin: *const clap_plugin) {
        // TODO: Does this get used? Is this only for the free-standing window extension? (which we
        //       don't implement) This wouldn't make any sense for embedded editors.
    }

    unsafe extern "C" fn ext_gui_hide(_plugin: *const clap_plugin) {
        // TODO: Same as the above
    }

    #[cfg(all(target_family = "unix", not(target_os = "macos")))]
    unsafe extern "C" fn ext_gui_x11_attach(
        plugin: *const clap_plugin,
        // TODO: Should we do anything with the display name?
        _display_name: *const c_char,
        window: c_ulong,
    ) -> bool {
        check_null_ptr!(false, plugin);
        // For this function we need the underlying Arc so we can pass it to the editor
        let wrapper = Arc::from_raw(plugin as *const Self);

        let result = {
            let mut editor_handle = wrapper.editor_handle.write();
            if editor_handle.is_none() {
                let handle = {
                    let mut handle = raw_window_handle::XcbHandle::empty();
                    handle.window = window as u32;
                    RawWindowHandle::Xcb(handle)
                };

                // This extension is only exposed when we have an editor
                *editor_handle = Some(wrapper.editor.as_ref().unwrap().spawn(
                    ParentWindowHandle { handle },
                    wrapper.clone().make_gui_context(),
                ));

                true
            } else {
                nih_debug_assert_failure!(
                    "Host tried to attach editor while the editor is already attached"
                );

                false
            }
        };

        // Leak the Arc again since we only needed a clone to pass to the GuiContext
        let _ = Arc::into_raw(wrapper);

        result
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" fn ext_gui_cocoa_attach(
        plugin: *const clap_plugin,
        ns_view: *mut c_void,
    ) -> bool {
        check_null_ptr!(false, plugin, ns_view);
        // For this function we need the underlying Arc so we can pass it to the editor
        let wrapper = Arc::from_raw(plugin as *const Self);

        let result = {
            let mut editor_handle = wrapper.editor_handle.write();
            if editor_handle.is_none() {
                let handle = {
                    let mut handle = raw_window_handle::AppKitHandle::empty();
                    handle.ns_view = ns_view;
                    RawWindowHandle::AppKit(handle)
                };

                // This extension is only exposed when we have an editor
                *editor_handle = Some(wrapper.editor.as_ref().unwrap().spawn(
                    ParentWindowHandle { handle },
                    wrapper.clone().make_gui_context(),
                ));

                true
            } else {
                nih_debug_assert_failure!(
                    "Host tried to attach editor while the editor is already attached"
                );

                false
            }
        };

        // Leak the Arc again since we only needed a clone to pass to the GuiContext
        let _ = Arc::into_raw(wrapper);

        result
    }

    #[cfg(target_os = "windows")]
    unsafe extern "C" fn ext_gui_win32_attach(
        plugin: *const clap_plugin,
        window: clap_hwnd,
    ) -> bool {
        check_null_ptr!(false, plugin, window);
        // For this function we need the underlying Arc so we can pass it to the editor
        let wrapper = Arc::from_raw(plugin as *const Self);

        let result = {
            let mut editor_handle = wrapper.editor_handle.write();
            if editor_handle.is_none() {
                let handle = {
                    let mut handle = raw_window_handle::Win32Handle::empty();
                    handle.hwnd = window;
                    RawWindowHandle::Win32(handle)
                };

                // This extension is only exposed when we have an editor
                *editor_handle = Some(wrapper.editor.as_ref().unwrap().spawn(
                    ParentWindowHandle { handle },
                    wrapper.clone().make_gui_context(),
                ));

                true
            } else {
                nih_debug_assert_failure!(
                    "Host tried to attach editor while the editor is already attached"
                );

                false
            }
        };

        // Leak the Arc again since we only needed a clone to pass to the GuiContext
        let _ = Arc::into_raw(wrapper);

        result
    }

    unsafe extern "C" fn ext_latency_get(plugin: *const clap_plugin) -> u32 {
        check_null_ptr!(0, plugin);
        let wrapper = &*(plugin as *const Self);

        wrapper.current_latency.load(Ordering::SeqCst)
    }

    unsafe extern "C" fn ext_params_count(plugin: *const clap_plugin) -> u32 {
        check_null_ptr!(0, plugin);
        let wrapper = &*(plugin as *const Self);

        // NOTE: We add a bypass parameter ourselves on index `plugin.param_hashes.len()`, so
        //       these indices are all off by one
        wrapper.param_hashes.len() as u32 + 1
    }

    unsafe extern "C" fn ext_params_get_info(
        plugin: *const clap_plugin,
        param_index: i32,
        param_info: *mut clap_param_info,
    ) -> bool {
        check_null_ptr!(false, plugin, param_info);
        let wrapper = &*(plugin as *const Self);

        // Parameter index `self.param_ids.len()` is our own bypass parameter
        if param_index < 0 || param_index > wrapper.param_hashes.len() as i32 {
            return false;
        }

        *param_info = std::mem::zeroed();

        // TODO: We don't use the cookies at this point. In theory this would be faster than the ID
        //       hashmap lookup, but for now we'll stay consistent with the VST3 implementation.
        let param_info = &mut *param_info;
        if param_index == wrapper.param_hashes.len() as i32 {
            param_info.id = *BYPASS_PARAM_HASH;
            param_info.flags = CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_BYPASS;
            param_info.cookie = ptr::null_mut();
            strlcpy(&mut param_info.name, "Bypass");
            strlcpy(&mut param_info.module, "");
            param_info.min_value = 0.0;
            param_info.max_value = 1.0;
            param_info.default_value = 0.0;
        } else {
            let param_hash = &wrapper.param_hashes[param_index as usize];
            let default_value = &wrapper.param_defaults_normalized[param_hash];
            let param_ptr = &wrapper.param_by_hash[param_hash];
            let step_count = param_ptr.step_count();

            param_info.id = *param_hash;
            param_info.flags = if step_count.is_some() {
                CLAP_PARAM_IS_STEPPED
            } else {
                0
            };
            param_info.cookie = ptr::null_mut();
            strlcpy(&mut param_info.name, param_ptr.name());
            strlcpy(&mut param_info.module, "");
            // We don't use the actual minimum and maximum values here because that would not scale
            // with skewed integer ranges. Instead, just treat all parameters as `[0, 1]` normalized
            // paramters multiplied by the step size.
            param_info.min_value = 0.0;
            // Stepped parameters are unnormalized float parameters since there's no separate step
            // range option
            // TODO: This should probably be encapsulated in some way so we don't forget about this in one place
            // TODO: Like with VST3, this won't actually do the correct thing with skewed stepped parameters
            param_info.max_value = step_count.unwrap_or(1) as f64;
            param_info.default_value = *default_value as f64 * step_count.unwrap_or(1) as f64;
        }

        true
    }

    unsafe extern "C" fn ext_params_get_value(
        plugin: *const clap_plugin,
        param_id: clap_id,
        value: *mut f64,
    ) -> bool {
        check_null_ptr!(false, plugin, value);
        let wrapper = &*(plugin as *const Self);

        if param_id == *BYPASS_PARAM_HASH {
            *value = if wrapper.bypass_state.load(Ordering::SeqCst) {
                1.0
            } else {
                0.0
            };
            true
        } else if let Some(param_ptr) = wrapper.param_by_hash.get(&param_id) {
            // TODO: As explained above, this may do strange things with skewed discrete parameters
            *value =
                param_ptr.normalized_value() as f64 * param_ptr.step_count().unwrap_or(1) as f64;
            true
        } else {
            false
        }
    }

    unsafe extern "C" fn ext_params_value_to_text(
        plugin: *const clap_plugin,
        param_id: clap_id,
        value: f64,
        display: *mut c_char,
        size: u32,
    ) -> bool {
        check_null_ptr!(false, plugin, display);
        let wrapper = &*(plugin as *const Self);

        let dest = std::slice::from_raw_parts_mut(display, size as usize);

        if param_id == *BYPASS_PARAM_HASH {
            if value > 0.5 {
                strlcpy(dest, "Bypassed")
            } else {
                strlcpy(dest, "Not Bypassed")
            }

            true
        } else if let Some(param_ptr) = wrapper.param_by_hash.get(&param_id) {
            strlcpy(
                dest,
                // CLAP does not have a separate unit, so we'll include the unit here
                &param_ptr.normalized_value_to_string(
                    value as f32 / param_ptr.step_count().unwrap_or(1) as f32,
                    true,
                ),
            );

            true
        } else {
            false
        }
    }

    unsafe extern "C" fn ext_params_text_to_value(
        plugin: *const clap_plugin,
        param_id: clap_id,
        display: *const c_char,
        value: *mut f64,
    ) -> bool {
        check_null_ptr!(false, plugin, display, value);
        let wrapper = &*(plugin as *const Self);

        let display = match CStr::from_ptr(display).to_str() {
            Ok(s) => s,
            Err(_) => return false,
        };

        if param_id == *BYPASS_PARAM_HASH {
            let normalized_valeu = match display {
                "Bypassed" => 1.0,
                "Not Bypassed" => 0.0,
                _ => return false,
            };
            *value = normalized_valeu;

            true
        } else if let Some(param_ptr) = wrapper.param_by_hash.get(&param_id) {
            let normalized_value = match param_ptr.string_to_normalized_value(display) {
                Some(v) => v as f64,
                None => return false,
            };
            *value = normalized_value * param_ptr.step_count().unwrap_or(1) as f64;

            true
        } else {
            false
        }
    }

    unsafe extern "C" fn ext_params_flush(
        plugin: *const clap_plugin,
        in_: *const clap_input_events,
        out: *const clap_output_events,
    ) {
        check_null_ptr!((), plugin);
        let wrapper = &*(plugin as *const Self);

        if !in_.is_null() {
            wrapper.handle_in_events(&*in_);
        }

        if !out.is_null() {
            wrapper.handle_out_events(&*out);
        }
    }

    unsafe extern "C" fn ext_state_save(
        plugin: *const clap_plugin,
        stream: *mut clap_ostream,
    ) -> bool {
        check_null_ptr!(false, plugin, stream);
        let wrapper = &*(plugin as *const Self);

        let serialized = state::serialize(
            wrapper.plugin.read().params(),
            &wrapper.param_by_hash,
            &wrapper.param_id_to_hash,
            BYPASS_PARAM_ID,
            &wrapper.bypass_state,
        );
        match serialized {
            Ok(serialized) => {
                // CLAP does not provide a way to tell how much data there is left in a stream, so
                // we need to prepend it to our actual state data.
                let length_bytes = (serialized.len() as u64).to_le_bytes();
                let num_length_bytes_written = ((*stream).write)(
                    stream,
                    length_bytes.as_ptr() as *const c_void,
                    length_bytes.len() as u64,
                );
                let num_bytes_written = ((*stream).write)(
                    stream,
                    serialized.as_ptr() as *const c_void,
                    serialized.len() as u64,
                );

                nih_debug_assert_eq!(num_length_bytes_written as usize, length_bytes.len());
                nih_debug_assert_eq!(num_bytes_written as usize, serialized.len());
                true
            }
            Err(err) => {
                nih_debug_assert_failure!("Could not save state: {}", err);
                false
            }
        }
    }

    unsafe extern "C" fn ext_state_load(
        plugin: *const clap_plugin,
        stream: *mut clap_istream,
    ) -> bool {
        check_null_ptr!(false, plugin, stream);
        let wrapper = &*(plugin as *const Self);

        // CLAP does not have a way to tell how much data there is left in a stream, so we've
        // prepended the size in front of our JSON state
        let mut length_bytes = [0; 8];
        let num_length_bytes_read = ((*stream).read)(
            stream,
            length_bytes.as_mut_ptr() as *mut c_void,
            length_bytes.len() as u64,
        );
        nih_debug_assert_eq!(num_length_bytes_read as usize, length_bytes.len());
        let length = u64::from_le_bytes(length_bytes);

        let mut read_buffer: Vec<u8> = Vec::with_capacity(length as usize);
        let num_bytes_read = ((*stream).read)(
            stream,
            read_buffer.as_mut_ptr() as *mut c_void,
            length as u64,
        );
        nih_debug_assert_eq!(num_bytes_read as u64, length);
        read_buffer.set_len(length as usize);

        let success = state::deserialize(
            &read_buffer,
            wrapper.plugin.read().params(),
            &wrapper.param_by_hash,
            &wrapper.param_id_to_hash,
            wrapper.current_buffer_config.load().as_ref(),
            BYPASS_PARAM_ID,
            &wrapper.bypass_state,
        );
        if !success {
            return false;
        }

        // Reinitialize the plugin after loading state so it can respond to the new parameter values
        let bus_config = wrapper.current_bus_config.load();
        if let Some(buffer_config) = wrapper.current_buffer_config.load() {
            wrapper.plugin.write().initialize(
                &bus_config,
                &buffer_config,
                &mut wrapper.make_process_context(),
            );
        }

        true
    }
}

/// Convenience function to query an extennsion from the host.
///
/// # Safety
///
/// The extension type `T` must match the extension's name `name`.
unsafe fn query_host_extension<T>(
    host_callback: &ClapPtr<clap_host>,
    name: *const c_char,
) -> Option<ClapPtr<T>> {
    let extension_ptr = (host_callback.get_extension)(&**host_callback, name);
    if !extension_ptr.is_null() {
        Some(ClapPtr::new(extension_ptr as *const T))
    } else {
        None
    }
}