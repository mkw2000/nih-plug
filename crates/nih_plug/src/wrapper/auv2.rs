#![allow(non_snake_case)]

use atomic_refcell::AtomicRefCell;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Protocol, Sel};
use objc::{class, msg_send, sel, sel_impl, Encode, Encoding};
use parking_lot::Mutex;
use std::borrow::Borrow;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void};
use std::mem;
use std::num::NonZeroU32;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::context::PluginApi;
use crate::event_loop::{EventLoop, MainThreadExecutor, OsEventLoop};
use crate::midi::MidiResult;
use crate::prelude::{
    AsyncExecutor, AudioIOLayout, AuxiliaryBuffers, BufferConfig, Editor, GuiContext, InitContext,
    MidiConfig, ParamFlags, ParamPtr, Params, ParentWindowHandle, PluginApi::Auv2,
    PluginNoteEvent, PluginState, ProcessContext, ProcessMode, ProcessStatus, TaskExecutor,
    Transport,
};
use crate::util::permit_alloc;
use crate::wrapper::state;
use crate::wrapper::util::buffer_management::{BufferManager, ChannelPointers};
use crate::wrapper::util::{
    clamp_input_event_timing, clamp_output_event_timing, hash_param_id, process_wrapper, strlcpy,
};

use crate::plugin::auv2::Auv2Plugin;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioUnitRender(
        in_unit: AudioUnit,
        io_action_flags: *mut AudioUnitRenderActionFlags,
        in_time_stamp: *const AudioTimeStamp,
        in_output_bus_number: u32,
        in_number_frames: u32,
        io_data: *mut AudioBufferList,
    ) -> OSStatus;

    fn AUParameterSet(
        in_sending_listener: *mut c_void,
        in_sending_object: *mut c_void,
        in_parameter: *const AudioUnitParameter,
        in_value: AudioUnitParameterValue,
        in_buffer_offset_in_frames: u32,
    ) -> OSStatus;

    fn AUEventListenerNotify(
        in_sending_listener: *mut c_void,
        in_sending_object: *mut c_void,
        in_event: *const AudioUnitEvent,
    ) -> OSStatus;
}

#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    fn MIDIPacketListInit(pktlist: *mut MIDIPacketList) -> *mut MIDIPacket;
    fn MIDIPacketListAdd(
        pktlist: *mut MIDIPacketList,
        list_size: usize,
        cur_packet: *mut MIDIPacket,
        time: u64,
        n_data: usize,
        data: *const u8,
    ) -> *mut MIDIPacket;
}

const DEFAULT_SAMPLE_RATE: f64 = 44_100.0;
const DEFAULT_MAX_FRAMES_PER_SLICE: u32 = 512;
const MAX_AUDIO_CHANNELS: usize = 64;
const PARAMETER_QUEUE_CAPACITY: usize = 2048;
const MIDI_EVENT_QUEUE_CAPACITY: usize = 2048;
const MIDI_PACKET_LIST_CAPACITY: usize = 65_536;
const MIDI_PACKET_CHUNK_SIZE: usize = 256;

const K_AUDIO_UNIT_ERR_INVALID_PROPERTY: OSStatus = -10879;
const K_AUDIO_UNIT_ERR_INVALID_PARAMETER: OSStatus = -10878;
const K_AUDIO_UNIT_ERR_INVALID_ELEMENT: OSStatus = -10877;
const K_AUDIO_UNIT_ERR_NO_CONNECTION: OSStatus = -10876;
const K_AUDIO_UNIT_ERR_FAILED_INITIALIZATION: OSStatus = -10875;
const K_AUDIO_UNIT_ERR_TOO_MANY_FRAMES_TO_PROCESS: OSStatus = -10874;
const K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED: OSStatus = -10868;
const K_AUDIO_UNIT_ERR_UNINITIALIZED: OSStatus = -10867;
const K_AUDIO_UNIT_ERR_INVALID_SCOPE: OSStatus = -10866;
const K_AUDIO_UNIT_ERR_PROPERTY_NOT_WRITABLE: OSStatus = -10865;
const K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT: OSStatus = -10863;
const K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE: OSStatus = -10851;
const K_AUDIO_UNIT_ERR_INITIALIZED: OSStatus = -10849;

const K_AUDIO_UNIT_SCOPE_GLOBAL: AudioUnitScope = 0;
const K_AUDIO_UNIT_SCOPE_INPUT: AudioUnitScope = 1;
const K_AUDIO_UNIT_SCOPE_OUTPUT: AudioUnitScope = 2;

const K_AUDIO_UNIT_PROPERTY_CLASS_INFO: AudioUnitPropertyID = 0;
const K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION: AudioUnitPropertyID = 1;
const K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE: AudioUnitPropertyID = 2;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST: AudioUnitPropertyID = 3;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO: AudioUnitPropertyID = 4;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: AudioUnitPropertyID = 8;
const K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT: AudioUnitPropertyID = 11;
const K_AUDIO_UNIT_PROPERTY_LATENCY: AudioUnitPropertyID = 12;
const K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS: AudioUnitPropertyID = 13;
const K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: AudioUnitPropertyID = 14;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS: AudioUnitPropertyID = 16;
const K_AUDIO_UNIT_PROPERTY_TAIL_TIME: AudioUnitPropertyID = 20;
const K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT: AudioUnitPropertyID = 21;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: AudioUnitPropertyID = 23;
const K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET: AudioUnitPropertyID = 28;
const K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS: AudioUnitPropertyID = 27;
const K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING: AudioUnitPropertyID = 29;
const K_AUDIO_UNIT_PROPERTY_COCOA_UI: AudioUnitPropertyID = 31;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE: AudioUnitPropertyID = 33;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_ID_NAME: AudioUnitPropertyID = 34;
const K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET: AudioUnitPropertyID = 36;
const K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING: AudioUnitPropertyID = 38;
const K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO: AudioUnitPropertyID = 47;
const K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK: AudioUnitPropertyID = 48;

const K_AUDIO_UNIT_INITIALIZE_SELECT: i16 = 0x0001;
const K_AUDIO_UNIT_UNINITIALIZE_SELECT: i16 = 0x0002;
const K_AUDIO_UNIT_GET_PROPERTY_INFO_SELECT: i16 = 0x0003;
const K_AUDIO_UNIT_GET_PROPERTY_SELECT: i16 = 0x0004;
const K_AUDIO_UNIT_SET_PROPERTY_SELECT: i16 = 0x0005;
const K_AUDIO_UNIT_GET_PARAMETER_SELECT: i16 = 0x0006;
const K_AUDIO_UNIT_SET_PARAMETER_SELECT: i16 = 0x0007;
const K_AUDIO_UNIT_ADD_PROPERTY_LISTENER_SELECT: i16 = 0x000A;
const K_AUDIO_UNIT_REMOVE_PROPERTY_LISTENER_SELECT: i16 = 0x000B;
const K_AUDIO_UNIT_RESET_SELECT: i16 = 0x0009;
const K_AUDIO_UNIT_RENDER_SELECT: i16 = 0x000E;
const K_AUDIO_UNIT_ADD_RENDER_NOTIFY_SELECT: i16 = 0x000F;
const K_AUDIO_UNIT_REMOVE_RENDER_NOTIFY_SELECT: i16 = 0x0010;
const K_AUDIO_UNIT_SCHEDULE_PARAMETERS_SELECT: i16 = 0x0011;
const K_AUDIO_UNIT_REMOVE_PROPERTY_LISTENER_WITH_USER_DATA_SELECT: i16 = 0x0012;
const K_MUSIC_DEVICE_MIDI_EVENT_SELECT: i16 = 0x0101;
const K_MUSIC_DEVICE_SYS_EX_SELECT: i16 = 0x0102;

const K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE: AudioUnitRenderActionFlags = 1 << 4;

const K_AUDIO_FORMAT_LINEAR_PCM: OSType = fourcc(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: AudioFormatFlags = 1 << 0;
#[cfg(target_endian = "big")]
const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: AudioFormatFlags = 1 << 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: AudioFormatFlags = 1 << 3;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: AudioFormatFlags = 1 << 5;
#[cfg(target_endian = "big")]
const K_AUDIO_FORMAT_FLAGS_NATIVE_ENDIAN: AudioFormatFlags = K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN;
#[cfg(not(target_endian = "big"))]
const K_AUDIO_FORMAT_FLAGS_NATIVE_ENDIAN: AudioFormatFlags = 0;
const K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED: AudioFormatFlags =
    K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED | K_AUDIO_FORMAT_FLAGS_NATIVE_ENDIAN;

const K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID: AudioTimeStampFlags = 1 << 0;

const K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC: AudioUnitParameterUnit = 0;
const K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED: AudioUnitParameterUnit = 1;
const K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN: AudioUnitParameterUnit = 2;
const K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT: AudioUnitParameterUnit = 3;
const K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS: AudioUnitParameterUnit = 4;
const K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ: AudioUnitParameterUnit = 8;
const K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS: AudioUnitParameterUnit = 13;
const K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN: AudioUnitParameterUnit = 14;
const K_AUDIO_UNIT_PARAMETER_UNIT_BPM: AudioUnitParameterUnit = 22;
const K_AUDIO_UNIT_PARAMETER_UNIT_MILLISECONDS: AudioUnitParameterUnit = 24;

const K_AUDIO_UNIT_PARAMETER_FLAG_VALUES_HAVE_STRINGS: AudioUnitParameterOptions = 1 << 21;
const K_AUDIO_UNIT_PARAMETER_FLAG_CAN_RAMP: AudioUnitParameterOptions = 1 << 25;
const K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING: AudioUnitParameterOptions = 1 << 27;
const K_AUDIO_UNIT_PARAMETER_FLAG_IS_READABLE: AudioUnitParameterOptions = 1 << 30;
const K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE: AudioUnitParameterOptions = 1 << 31;
const K_AUDIO_UNIT_PARAMETER_FLAG_GLOBAL: AudioUnitParameterOptions = 1 << 0;
const K_AUDIO_UNIT_PARAMETER_FLAG_CF_NAME_RELEASE: AudioUnitParameterOptions = 1 << 4;
const K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE: u32 = 1;
const K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE: u32 = 2;

const CLASS_INFO_VERSION_KEY: &str = "version";
const CLASS_INFO_TYPE_KEY: &str = "type";
const CLASS_INFO_SUBTYPE_KEY: &str = "subtype";
const CLASS_INFO_MANUFACTURER_KEY: &str = "manufacturer";
const CLASS_INFO_NAME_KEY: &str = "name";
const CLASS_INFO_PRESET_NUMBER_KEY: &str = "preset-number";
const CLASS_INFO_DATA_KEY: &str = "data";
pub const NIH_AUV2_FACTORY_SYMBOL: &str = "NihAudioUnitFactory";
pub const NIH_AUV2_METADATA_SYMBOL: &str = "NihAudioUnitBundlerMetadata";
const COCOA_VIEW_STATE_IVAR: &str = "nihStatePtr";
static COCOA_CLASS_REGISTRATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static AUDIO_UNIT_INSTANCE_WRAPPERS: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

type OSStatus = i32;
type OSType = u32;
type AudioComponentInstance = *mut c_void;
type AudioUnit = AudioComponentInstance;
type AudioUnitPropertyID = u32;
type AudioUnitScope = u32;
type AudioUnitElement = u32;
type AudioUnitParameterID = u32;
type AudioUnitParameterValue = f32;
type AudioUnitRenderActionFlags = u32;
type AudioFormatFlags = u32;
type AudioTimeStampFlags = u32;
type AudioUnitParameterUnit = u32;
type AudioUnitParameterOptions = u32;
type AudioComponentMethod = *const c_void;
type Boolean = u8;
type CFURLRef = *const c_void;
type AudioUnitPropertyListenerProc = unsafe extern "C" fn(
    *mut c_void,
    AudioUnit,
    AudioUnitPropertyID,
    AudioUnitScope,
    AudioUnitElement,
);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioComponentDescription {
    pub componentType: OSType,
    pub componentSubType: OSType,
    pub componentManufacturer: OSType,
    pub componentFlags: u32,
    pub componentFlagsMask: u32,
}

#[repr(C)]
pub struct AudioComponentPlugInInterface {
    pub Open: Option<unsafe extern "C" fn(*mut c_void, AudioComponentInstance) -> OSStatus>,
    pub Close: Option<unsafe extern "C" fn(*mut c_void) -> OSStatus>,
    pub Lookup: Option<unsafe extern "C" fn(i16) -> AudioComponentMethod>,
    pub reserved: *mut c_void,
}

unsafe impl Send for AudioComponentPlugInInterface {}
unsafe impl Sync for AudioComponentPlugInInterface {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

#[repr(C)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SMPTETime {
    pub mSubframes: i16,
    pub mSubframeDivisor: i16,
    pub mCounter: u32,
    pub mType: u32,
    pub mFlags: u32,
    pub mHours: i16,
    pub mMinutes: i16,
    pub mSeconds: i16,
    pub mFrames: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: SMPTETime,
    pub mFlags: AudioTimeStampFlags,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: OSType,
    pub mFormatFlags: AudioFormatFlags,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitConnection {
    pub sourceAudioUnit: AudioUnit,
    pub sourceOutputNumber: u32,
    pub destInputNumber: u32,
}

unsafe impl Send for AudioUnitConnection {}
unsafe impl Sync for AudioUnitConnection {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AUChannelInfo {
    pub inChannels: i16,
    pub outChannels: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AURenderCallbackStruct {
    pub inputProc: Option<AURenderCallback>,
    pub inputProcRefCon: *mut c_void,
}

unsafe impl Send for AURenderCallbackStruct {}
unsafe impl Sync for AURenderCallbackStruct {}

pub type AURenderCallback = unsafe extern "C" fn(
    *mut c_void,
    *mut AudioUnitRenderActionFlags,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;

#[derive(Clone, Copy)]
struct PropertyListener {
    property_id: AudioUnitPropertyID,
    proc: AudioUnitPropertyListenerProc,
    user_data: usize,
}

pub type HostCallbackGetBeatAndTempo =
    unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> OSStatus;
pub type HostCallbackGetMusicalTimeLocation =
    unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> OSStatus;
pub type HostCallbackGetTransportState = unsafe extern "C" fn(
    *mut c_void,
    *mut Boolean,
    *mut Boolean,
    *mut f64,
    *mut Boolean,
    *mut f64,
    *mut f64,
) -> OSStatus;
pub type HostCallbackGetTransportState2 = unsafe extern "C" fn(
    *mut c_void,
    *mut Boolean,
    *mut Boolean,
    *mut Boolean,
    *mut f64,
    *mut Boolean,
    *mut f64,
    *mut f64,
) -> OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostCallbackInfo {
    pub hostUserData: *mut c_void,
    pub beatAndTempoProc: Option<HostCallbackGetBeatAndTempo>,
    pub musicalTimeLocationProc: Option<HostCallbackGetMusicalTimeLocation>,
    pub transportStateProc: Option<HostCallbackGetTransportState>,
    pub transportStateProc2: Option<HostCallbackGetTransportState2>,
}

unsafe impl Send for HostCallbackInfo {}
unsafe impl Sync for HostCallbackInfo {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameterInfo {
    pub name: [c_char; 52],
    pub unitName: CFStringRef,
    pub clumpID: u32,
    pub cfNameString: CFStringRef,
    pub unit: AudioUnitParameterUnit,
    pub minValue: AudioUnitParameterValue,
    pub maxValue: AudioUnitParameterValue,
    pub defaultValue: AudioUnitParameterValue,
    pub flags: AudioUnitParameterOptions,
}

#[repr(C)]
pub struct AUPreset {
    pub presetNumber: i32,
    pub presetName: CFStringRef,
}

#[repr(C)]
pub struct AudioUnitParameterStringFromValue {
    pub inParamID: AudioUnitParameterID,
    pub inValue: *const AudioUnitParameterValue,
    pub outString: CFStringRef,
}

#[repr(C)]
pub struct AudioUnitParameterValueFromString {
    pub inParamID: AudioUnitParameterID,
    pub inString: CFStringRef,
    pub outValue: AudioUnitParameterValue,
}

#[repr(C)]
pub struct AudioUnitParameterIDName {
    pub inID: AudioUnitParameterID,
    pub inDesiredLength: i32,
    pub outName: CFStringRef,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameterEvent {
    pub scope: AudioUnitScope,
    pub element: AudioUnitElement,
    pub parameter: AudioUnitParameterID,
    pub eventType: u32,
    pub eventValues: AudioUnitParameterEventValues,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union AudioUnitParameterEventValues {
    pub ramp: AudioUnitParameterRamp,
    pub immediate: AudioUnitParameterImmediate,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameterRamp {
    pub startBufferOffset: i32,
    pub durationInFrames: u32,
    pub startValue: AudioUnitParameterValue,
    pub endValue: AudioUnitParameterValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameterImmediate {
    pub bufferOffset: u32,
    pub value: AudioUnitParameterValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitParameter {
    pub mAudioUnit: AudioUnit,
    pub mParameterID: AudioUnitParameterID,
    pub mScope: AudioUnitScope,
    pub mElement: AudioUnitElement,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union AudioUnitEventArgument {
    pub mParameter: AudioUnitParameter,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitEvent {
    pub mEventType: u32,
    pub mArgument: AudioUnitEventArgument,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioUnitCocoaViewInfo {
    pub mCocoaAUViewBundleLocation: CFURLRef,
    pub mCocoaAUViewClass: [CFStringRef; 1],
}

pub type AUMIDIOutputCallback = unsafe extern "C" fn(
    *mut c_void,
    *const AudioTimeStamp,
    u32,
    *const MIDIPacketList,
) -> OSStatus;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AUMIDIOutputCallbackStruct {
    pub midiOutputCallback: Option<AUMIDIOutputCallback>,
    pub userData: *mut c_void,
}

unsafe impl Send for AUMIDIOutputCallbackStruct {}
unsafe impl Sync for AUMIDIOutputCallbackStruct {}

#[repr(C)]
pub struct MIDIPacket {
    pub timeStamp: u64,
    pub length: u16,
    pub data: [u8; 256],
}

#[repr(C)]
pub struct MIDIPacketList {
    pub numPackets: u32,
    pub packet: [MIDIPacket; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

#[cfg(target_pointer_width = "64")]
const NSPOINT_ENCODING: &str = "{CGPoint=dd}";
#[cfg(target_pointer_width = "64")]
const NSSIZE_ENCODING: &str = "{CGSize=dd}";
#[cfg(target_pointer_width = "64")]
const NSRECT_ENCODING: &str = "{CGRect={CGPoint=dd}{CGSize=dd}}";

#[cfg(not(target_pointer_width = "64"))]
const NSPOINT_ENCODING: &str = "{_NSPoint=ff}";
#[cfg(not(target_pointer_width = "64"))]
const NSSIZE_ENCODING: &str = "{_NSSize=ff}";
#[cfg(not(target_pointer_width = "64"))]
const NSRECT_ENCODING: &str = "{_NSRect={_NSPoint=ff}{_NSSize=ff}}";

unsafe impl Encode for NSPoint {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str(NSPOINT_ENCODING) }
    }
}

unsafe impl Encode for NSSize {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str(NSSIZE_ENCODING) }
    }
}

unsafe impl Encode for NSRect {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str(NSRECT_ENCODING) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Auv2BundlerMetadata {
    pub type_code: [u8; 4],
    pub subtype_code: [u8; 4],
    pub manufacturer_code: [u8; 4],
    pub name_ptr: *const u8,
    pub name_len: usize,
    pub vendor_ptr: *const u8,
    pub vendor_len: usize,
    pub version_ptr: *const u8,
    pub version_len: usize,
    pub sandbox_safe: bool,
}

unsafe impl Send for Auv2BundlerMetadata {}
unsafe impl Sync for Auv2BundlerMetadata {}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Auv2BundlerMetadataList {
    pub ptr: *const Auv2BundlerMetadata,
    pub len: usize,
}

unsafe impl Send for Auv2BundlerMetadataList {}
unsafe impl Sync for Auv2BundlerMetadataList {}

impl Auv2BundlerMetadata {
    pub fn for_plugin<P: Auv2Plugin>() -> Self {
        Self {
            type_code: P::AUV2_TYPE,
            subtype_code: P::AUV2_SUBTYPE,
            manufacturer_code: P::AUV2_MANUFACTURER,
            name_ptr: P::NAME.as_ptr(),
            name_len: P::NAME.len(),
            vendor_ptr: P::VENDOR.as_ptr(),
            vendor_len: P::VENDOR.len(),
            version_ptr: P::VERSION.as_ptr(),
            version_len: P::VERSION.len(),
            sandbox_safe: P::AUV2_SANDBOX_SAFE,
        }
    }

    pub fn matches(&self, desc: &AudioComponentDescription) -> bool {
        desc.componentType == fourcc(self.type_code)
            && desc.componentSubType == fourcc(self.subtype_code)
            && desc.componentManufacturer == fourcc(self.manufacturer_code)
    }
}

struct WrapperInitContext<'a, P: Auv2Plugin> {
    wrapper: &'a Wrapper<P>,
}

struct WrapperProcessContext<'a, P: Auv2Plugin> {
    wrapper: &'a Wrapper<P>,
    input_events: &'a [PluginNoteEvent<P>],
    input_event_idx: usize,
    output_events: &'a mut Vec<PluginNoteEvent<P>>,
    event_timing_offset: u32,
    transport: Transport,
}

struct WrapperGuiContext<P: Auv2Plugin> {
    wrapper: Arc<Wrapper<P>>,
}

enum Task<P: Auv2Plugin> {
    PluginTask(P::BackgroundTask),
    ParameterValuesChanged,
    ParameterValueChanged(u32, f32),
}

#[derive(Clone, Copy)]
struct ScheduledParameterChange {
    param_hash: u32,
    sample_offset: u32,
    normalized_value: f32,
}

#[repr(C)]
struct FixedAudioBufferList {
    mNumberBuffers: u32,
    mBuffers: [AudioBuffer; MAX_AUDIO_CHANNELS],
}

struct BusIOBuffers {
    storage: Vec<Vec<f32>>,
    channel_pointers: Vec<*mut f32>,
    buffer_list: FixedAudioBufferList,
}

struct OutputBusBuffers {
    storage: Vec<Vec<f32>>,
    channel_pointers: Vec<*mut f32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RenderCycleKey {
    sample_time_bits: Option<u64>,
    host_time: u64,
    num_frames: u32,
}

struct RenderState {
    last_cycle_key: Option<RenderCycleKey>,
    rendered_output_buses: Vec<bool>,
}

struct CocoaViewState<P: Auv2Plugin> {
    view: *mut Object,
    wrapper: Arc<Wrapper<P>>,
    _gui_context: Arc<WrapperGuiContext<P>>,
    _editor_handle: Box<dyn Any + Send>,
}

struct IOBuffers<P: Auv2Plugin> {
    input_busses: Vec<BusIOBuffers>,
    output_busses: Vec<OutputBusBuffers>,
    aux_input_pointers: Vec<Option<ChannelPointers>>,
    aux_output_pointers: Vec<Option<ChannelPointers>>,
    scratch_parameter_changes: Vec<ScheduledParameterChange>,
    scratch_input_events: Vec<PluginNoteEvent<P>>,
    scratch_block_input_events: Vec<PluginNoteEvent<P>>,
    scratch_output_events: Vec<PluginNoteEvent<P>>,
    midi_packet_list_storage: Box<[u8]>,
}

unsafe impl<P: Auv2Plugin> Send for IOBuffers<P> {}
unsafe impl<P: Auv2Plugin> Sync for IOBuffers<P> {}

impl<P: Auv2Plugin> Drop for CocoaViewState<P> {
    fn drop(&mut self) {
        if self.wrapper.cocoa_view.load() == self.view as usize {
            self.wrapper.cocoa_view.store(0);
        }

        self.wrapper.open_editor_count.fetch_sub(1, Ordering::Relaxed);
    }
}

#[repr(C)]
pub struct Wrapper<P: Auv2Plugin> {
    interface: AudioComponentPlugInInterface,

    plugin: Mutex<P>,
    task_executor: Mutex<TaskExecutor<P>>,
    params: Arc<dyn Params>,
    editor: AtomicRefCell<Option<Mutex<Box<dyn Editor>>>>,
    open_editor_count: AtomicU32,

    event_loop: AtomicRefCell<Option<OsEventLoop<Task<P>, Self>>>,

    supported_audio_io_layouts: Vec<AudioIOLayout>,
    supported_channel_infos: Vec<AUChannelInfo>,

    current_audio_io_layout: AtomicCell<AudioIOLayout>,
    component_instance: AtomicCell<usize>,
    cocoa_view: AtomicCell<usize>,
    sample_rate: AtomicCell<f64>,
    maximum_frames_per_slice: AtomicU32,
    current_buffer_config: AtomicCell<Option<BufferConfig>>,
    current_latency: AtomicU32,
    is_initialized: AtomicBool,
    bypass_enabled: AtomicBool,
    input_callbacks: Vec<AtomicCell<Option<AURenderCallbackStruct>>>,
    input_connections: Vec<AtomicCell<Option<AudioUnitConnection>>>,
    host_callbacks: AtomicCell<Option<HostCallbackInfo>>,
    midi_output_callback: AtomicCell<Option<AUMIDIOutputCallbackStruct>>,
    in_place_processing: AtomicBool,
    property_listeners: Mutex<Vec<PropertyListener>>,

    buffer_manager: AtomicRefCell<BufferManager>,
    io_buffers: AtomicRefCell<IOBuffers<P>>,
    render_state: AtomicRefCell<RenderState>,
    pending_parameter_changes: ArrayQueue<ScheduledParameterChange>,
    pending_input_events: ArrayQueue<PluginNoteEvent<P>>,

    param_hashes: Vec<u32>,
    param_by_hash: HashMap<u32, ParamPtr>,
    param_id_by_hash: HashMap<u32, String>,
    param_id_to_hash: HashMap<String, u32>,
    param_ptr_to_hash: HashMap<ParamPtr, u32>,
    bypass_param: Option<ParamPtr>,
}

impl<P: Auv2Plugin> Wrapper<P> {
    pub fn new() -> Option<Arc<Self>> {
        let supported_audio_io_layouts = Self::supported_audio_io_layouts()?;
        let supported_channel_infos = Self::supported_channel_infos(&supported_audio_io_layouts);
        let initial_audio_io_layout = *supported_audio_io_layouts.first()?;
        let max_input_bus_count = supported_audio_io_layouts
            .iter()
            .map(|layout| Self::bus_count_for_layout(*layout, K_AUDIO_UNIT_SCOPE_INPUT) as usize)
            .max()
            .unwrap_or(0);

        let mut plugin = P::default();
        let task_executor = plugin.task_executor();
        let params = plugin.params();

        let param_id_hashes_ptrs: Vec<_> = params
            .param_map()
            .into_iter()
            .map(|(id, ptr, _group)| {
                let hash = hash_param_id(&id);
                (id, hash, ptr)
            })
            .collect();

        if cfg!(debug_assertions) {
            let param_ids: HashSet<_> = param_id_hashes_ptrs
                .iter()
                .map(|(id, _, _)| id.clone())
                .collect();
            nih_debug_assert_eq!(
                param_ids.len(),
                param_id_hashes_ptrs.len(),
                "The plugin has duplicate parameter IDs, weird things may happen."
            );
        }

        let param_hashes = param_id_hashes_ptrs.iter().map(|(_, hash, _)| *hash).collect();
        let param_by_hash = param_id_hashes_ptrs
            .iter()
            .map(|(_, hash, ptr)| (*hash, *ptr))
            .collect();
        let param_id_by_hash = param_id_hashes_ptrs
            .iter()
            .map(|(id, hash, _)| (*hash, id.clone()))
            .collect();
        let param_id_to_hash = param_id_hashes_ptrs
            .iter()
            .map(|(id, hash, _)| (id.clone(), *hash))
            .collect();
        let param_ptr_to_hash = param_id_hashes_ptrs
            .iter()
            .map(|(_, hash, ptr)| (*ptr, *hash))
            .collect();
        let bypass_param = param_id_hashes_ptrs.iter().find_map(|(_, _, ptr)| {
            let flags = unsafe { ptr.flags() };
            if flags.contains(ParamFlags::BYPASS) {
                Some(*ptr)
            } else {
                None
            }
        });

        let wrapper = Arc::new_cyclic(|weak| Self {
            interface: AudioComponentPlugInInterface {
                Open: Some(open::<P>),
                Close: Some(close::<P>),
                Lookup: Some(lookup::<P>),
                reserved: ptr::null_mut(),
            },

            plugin: Mutex::new(plugin),
            task_executor: Mutex::new(task_executor),
            params,
            editor: AtomicRefCell::new(None),
            open_editor_count: AtomicU32::new(0),

            event_loop: AtomicRefCell::new(Some(OsEventLoop::new_and_spawn(weak.clone()))),

            supported_audio_io_layouts,
            supported_channel_infos,

            current_audio_io_layout: AtomicCell::new(initial_audio_io_layout),
            component_instance: AtomicCell::new(0),
            cocoa_view: AtomicCell::new(0),
            sample_rate: AtomicCell::new(DEFAULT_SAMPLE_RATE),
            maximum_frames_per_slice: AtomicU32::new(DEFAULT_MAX_FRAMES_PER_SLICE),
            current_buffer_config: AtomicCell::new(None),
            current_latency: AtomicU32::new(0),
            is_initialized: AtomicBool::new(false),
            bypass_enabled: AtomicBool::new(false),
            input_callbacks: (0..max_input_bus_count)
                .map(|_| AtomicCell::new(None))
                .collect(),
            input_connections: (0..max_input_bus_count)
                .map(|_| AtomicCell::new(None))
                .collect(),
            host_callbacks: AtomicCell::new(None),
            midi_output_callback: AtomicCell::new(None),
            in_place_processing: AtomicBool::new(Self::can_process_in_place(initial_audio_io_layout)),
            property_listeners: Mutex::new(Vec::new()),

            buffer_manager: AtomicRefCell::new(BufferManager::for_audio_io_layout(
                0,
                AudioIOLayout::default(),
            )),
            io_buffers: AtomicRefCell::new(IOBuffers::new(0, initial_audio_io_layout)),
            render_state: AtomicRefCell::new(RenderState::new::<P>(initial_audio_io_layout)),
            pending_parameter_changes: ArrayQueue::new(PARAMETER_QUEUE_CAPACITY),
            pending_input_events: ArrayQueue::new(MIDI_EVENT_QUEUE_CAPACITY),

            param_hashes,
            param_by_hash,
            param_id_by_hash,
            param_id_to_hash,
            param_ptr_to_hash,
            bypass_param,
        });

        *wrapper.editor.borrow_mut() = wrapper
            .plugin
            .lock()
            .editor(AsyncExecutor {
                execute_background: Arc::new({
                    let wrapper = Arc::downgrade(&wrapper);
                    move |task| {
                        let Some(wrapper) = wrapper.upgrade() else {
                            return;
                        };

                        let task_posted = wrapper.schedule_background(Task::PluginTask(task));
                        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
                    }
                }),
                execute_gui: Arc::new({
                    let wrapper = Arc::downgrade(&wrapper);
                    move |task| {
                        let Some(wrapper) = wrapper.upgrade() else {
                            return;
                        };

                        let task_posted = wrapper.schedule_gui(Task::PluginTask(task));
                        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
                    }
                }),
            })
            .map(Mutex::new);

        Some(wrapper)
    }

    fn supported_audio_io_layouts() -> Option<Vec<AudioIOLayout>> {
        let layouts: Vec<_> = if P::AUDIO_IO_LAYOUTS.is_empty() {
            vec![AudioIOLayout::default()]
        } else {
            P::AUDIO_IO_LAYOUTS
            .iter()
            .copied()
            .filter(|layout| {
                layout
                        .main_input_channels
                        .map(NonZeroU32::get)
                        .unwrap_or(0)
                        <= layout
                            .main_output_channels
                            .map(NonZeroU32::get)
                            .unwrap_or(u32::MAX)
                    && layout
                        .main_input_channels
                        .map(NonZeroU32::get)
                        .unwrap_or(0) as usize
                        <= MAX_AUDIO_CHANNELS
                    && layout
                        .main_output_channels
                        .map(NonZeroU32::get)
                        .unwrap_or(0) as usize
                        <= MAX_AUDIO_CHANNELS
                    && layout
                        .aux_input_ports
                        .iter()
                        .all(|channels| channels.get() as usize <= MAX_AUDIO_CHANNELS)
                    && layout
                        .aux_output_ports
                        .iter()
                        .all(|channels| channels.get() as usize <= MAX_AUDIO_CHANNELS)
            })
            .collect()
        };

        let first_layout = layouts.first()?;
        let has_input = first_layout.main_input_channels.is_some();
        let has_output = first_layout.main_output_channels.is_some();
        if layouts.iter().any(|layout| {
            layout.main_input_channels.is_some() != has_input
                || layout.main_output_channels.is_some() != has_output
        }) {
            nih_debug_assert_failure!(
                "The AUv2 wrapper currently requires all supported layouts to agree on whether \
                 they have a main input and output bus."
            );
            return None;
        }

        Some(layouts)
    }

    fn supported_channel_infos(layouts: &[AudioIOLayout]) -> Vec<AUChannelInfo> {
        let mut infos: Vec<_> = layouts
            .iter()
            .map(|layout| AUChannelInfo {
                inChannels: layout
                    .main_input_channels
                    .map(NonZeroU32::get)
                    .unwrap_or(0) as i16,
                outChannels: layout
                    .main_output_channels
                    .map(NonZeroU32::get)
                    .unwrap_or(0) as i16,
            })
            .collect();
        infos.sort_by_key(|info| (info.inChannels, info.outChannels));
        infos.dedup_by_key(|info| (info.inChannels, info.outChannels));
        infos
    }

    fn bus_count_for_layout(layout: AudioIOLayout, scope: AudioUnitScope) -> u32 {
        match scope {
            K_AUDIO_UNIT_SCOPE_INPUT => {
                u32::from(layout.main_input_channels.is_some()) + layout.aux_input_ports.len() as u32
            }
            K_AUDIO_UNIT_SCOPE_OUTPUT => {
                u32::from(layout.main_output_channels.is_some())
                    + layout.aux_output_ports.len() as u32
            }
            _ => 0,
        }
    }

    fn bus_count(&self, scope: AudioUnitScope) -> u32 {
        Self::bus_count_for_layout(self.current_audio_io_layout.load(), scope)
    }

    fn bus_channels_for_layout(
        layout: AudioIOLayout,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Option<u32> {
        let element = element as usize;
        match scope {
            K_AUDIO_UNIT_SCOPE_INPUT => match (
                layout.main_input_channels.map(NonZeroU32::get),
                layout.aux_input_ports,
            ) {
                (Some(main_channels), _) if element == 0 => Some(main_channels),
                (Some(_), aux_ports) => aux_ports.get(element - 1).map(|channels| channels.get()),
                (None, aux_ports) => aux_ports.get(element).map(|channels| channels.get()),
            },
            K_AUDIO_UNIT_SCOPE_OUTPUT => match (
                layout.main_output_channels.map(NonZeroU32::get),
                layout.aux_output_ports,
            ) {
                (Some(main_channels), _) if element == 0 => Some(main_channels),
                (Some(_), aux_ports) => aux_ports.get(element - 1).map(|channels| channels.get()),
                (None, aux_ports) => aux_ports.get(element).map(|channels| channels.get()),
            },
            _ => None,
        }
    }

    fn bus_channels(&self, scope: AudioUnitScope, element: AudioUnitElement) -> Option<u32> {
        Self::bus_channels_for_layout(self.current_audio_io_layout.load(), scope, element)
    }

    fn bus_channel_counts_for_layout(layout: AudioIOLayout, scope: AudioUnitScope) -> Vec<u32> {
        let mut counts = Vec::with_capacity(Self::bus_count_for_layout(layout, scope) as usize);
        match scope {
            K_AUDIO_UNIT_SCOPE_INPUT => {
                if let Some(channels) = layout.main_input_channels {
                    counts.push(channels.get());
                }
                counts.extend(layout.aux_input_ports.iter().map(|channels| channels.get()));
            }
            K_AUDIO_UNIT_SCOPE_OUTPUT => {
                if let Some(channels) = layout.main_output_channels {
                    counts.push(channels.get());
                }
                counts.extend(layout.aux_output_ports.iter().map(|channels| channels.get()));
            }
            _ => (),
        }

        counts
    }

    fn can_process_in_place(_layout: AudioIOLayout) -> bool {
        // This wrapper renders through internal bus storage and copies into the
        // host's output buffers after processing. If the AU reports in-place
        // support, hosts such as auval may legally provide a null output buffer
        // and expect the input buffer to hold the rendered output. The wrapper
        // does not currently implement that code path, so advertise out-of-place
        // processing only.
        false
    }

    fn match_audio_io_layout_by_busses(
        &self,
        input_channels: &[u32],
        output_channels: &[u32],
    ) -> Option<AudioIOLayout> {
        self.supported_audio_io_layouts
            .iter()
            .copied()
            .find(|layout| {
                Self::bus_channel_counts_for_layout(*layout, K_AUDIO_UNIT_SCOPE_INPUT)
                    == input_channels
                    && Self::bus_channel_counts_for_layout(*layout, K_AUDIO_UNIT_SCOPE_OUTPUT)
                        == output_channels
            })
    }

    fn match_audio_io_layout_for_stream_format(
        &self,
        scope: AudioUnitScope,
        input_channels: &[u32],
        output_channels: &[u32],
    ) -> Option<AudioIOLayout> {
        if let Some(layout) = self.match_audio_io_layout_by_busses(input_channels, output_channels) {
            return Some(layout);
        }

        // Hosts change input and output stream formats independently. Accept the
        // side that was just changed and move the other side to a compatible
        // supported layout instead of rejecting transient unsupported pairs such
        // as 2-in/1-out while the host is moving from mono to stereo.
        self.supported_audio_io_layouts
            .iter()
            .copied()
            .find(|layout| {
                let layout_input =
                    Self::bus_channel_counts_for_layout(*layout, K_AUDIO_UNIT_SCOPE_INPUT);
                let layout_output =
                    Self::bus_channel_counts_for_layout(*layout, K_AUDIO_UNIT_SCOPE_OUTPUT);

                match scope {
                    K_AUDIO_UNIT_SCOPE_INPUT => layout_input == input_channels,
                    K_AUDIO_UNIT_SCOPE_OUTPUT => layout_output == output_channels,
                    _ => false,
                }
            })
    }

    fn make_init_context(&self) -> WrapperInitContext<'_, P> {
        WrapperInitContext { wrapper: self }
    }

    fn make_process_context<'a>(
        &'a self,
        input_events: &'a [PluginNoteEvent<P>],
        output_events: &'a mut Vec<PluginNoteEvent<P>>,
        event_timing_offset: u32,
        transport: Transport,
    ) -> WrapperProcessContext<'a, P> {
        WrapperProcessContext {
            wrapper: self,
            input_events,
            input_event_idx: 0,
            output_events,
            event_timing_offset,
            transport,
        }
    }

    fn make_gui_context(self: Arc<Self>) -> Arc<WrapperGuiContext<P>> {
        Arc::new(WrapperGuiContext { wrapper: self })
    }

    fn schedule_background(&self, task: Task<P>) -> bool {
        self.event_loop
            .borrow()
            .as_ref()
            .expect("Missing event loop")
            .schedule_background(task)
    }

    fn schedule_gui(&self, task: Task<P>) -> bool {
        self.event_loop
            .borrow()
            .as_ref()
            .expect("Missing event loop")
            .schedule_gui(task)
    }

    fn notify_property_listeners(
        &self,
        property_id: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) {
        let listeners: Vec<_> = self
            .property_listeners
            .lock()
            .iter()
            .copied()
            .filter(|listener| listener.property_id == property_id)
            .collect();
        let instance = self.component_instance.load() as AudioUnit;

        for listener in listeners {
            unsafe {
                (listener.proc)(
                    listener.user_data as *mut c_void,
                    instance,
                    property_id,
                    scope,
                    element,
                );
            }
        }
    }

    fn set_latency_samples(&self, samples: u32) {
        self.current_latency.store(samples, Ordering::Relaxed);
    }

    fn get_state_object(&self) -> PluginState {
        unsafe {
            state::serialize_object::<P>(
                self.params.clone(),
                state::make_params_iter(&self.param_by_hash, &self.param_id_to_hash),
            )
        }
    }

    fn set_state_object_from_gui(&self, mut state: PluginState) {
        let _ = self.set_state_inner(&mut state);
    }

    fn is_bypassed(&self) -> bool {
        if let Some(param) = self.bypass_param {
            unsafe { param.modulated_plain_value() >= 0.5 }
        } else {
            self.bypass_enabled.load(Ordering::Relaxed)
        }
    }

    fn current_stream_format(
        &self,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<AudioStreamBasicDescription, OSStatus> {
        let channels =
            self.bus_channels(scope, element)
                .ok_or(K_AUDIO_UNIT_ERR_INVALID_ELEMENT)?;

        Ok(make_stream_format(self.sample_rate.load(), channels))
    }

    fn set_stream_format(
        &self,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        stream_format: &AudioStreamBasicDescription,
    ) -> OSStatus {
        if self.is_initialized.load(Ordering::Relaxed) {
            return K_AUDIO_UNIT_ERR_INITIALIZED;
        }
        if !stream_format_is_supported(stream_format) {
            return K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED;
        }

        let current_layout = self.current_audio_io_layout.load();
        let mut input_channels =
            Self::bus_channel_counts_for_layout(current_layout, K_AUDIO_UNIT_SCOPE_INPUT);
        let mut output_channels =
            Self::bus_channel_counts_for_layout(current_layout, K_AUDIO_UNIT_SCOPE_OUTPUT);
        let target_channels = stream_format.mChannelsPerFrame;
        match scope {
            K_AUDIO_UNIT_SCOPE_INPUT => {
                let Some(channels) = input_channels.get_mut(element as usize) else {
                    return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
                };
                *channels = target_channels;
            }
            K_AUDIO_UNIT_SCOPE_OUTPUT => {
                let Some(channels) = output_channels.get_mut(element as usize) else {
                    return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
                };
                *channels = target_channels;
            }
            _ => return K_AUDIO_UNIT_ERR_INVALID_SCOPE,
        };
        let new_layout =
            self.match_audio_io_layout_for_stream_format(scope, &input_channels, &output_channels);

        match new_layout {
            Some(layout) => {
                self.current_audio_io_layout.store(layout);
                self.in_place_processing
                    .store(Self::can_process_in_place(layout), Ordering::Relaxed);
                self.sample_rate.store(stream_format.mSampleRate);
                0
            }
            None => K_AUDIO_UNIT_ERR_FORMAT_NOT_SUPPORTED,
        }
    }

    fn initialize_inner(&self) -> OSStatus {
        if self.is_initialized.swap(true, Ordering::SeqCst) {
            return K_AUDIO_UNIT_ERR_INITIALIZED;
        }

        let buffer_config = BufferConfig {
            sample_rate: self.sample_rate.load() as f32,
            min_buffer_size: None,
            max_buffer_size: self.maximum_frames_per_slice.load(Ordering::Relaxed),
            process_mode: ProcessMode::Realtime,
        };
        let audio_io_layout = self.current_audio_io_layout.load();

        for param in self.param_by_hash.values() {
            unsafe { param.update_smoother(buffer_config.sample_rate, true) };
        }

        {
            let mut init_context = self.make_init_context();
            let mut plugin = self.plugin.lock();
            if !plugin.initialize(&audio_io_layout, &buffer_config, &mut init_context) {
                self.is_initialized.store(false, Ordering::SeqCst);
                return K_AUDIO_UNIT_ERR_FAILED_INITIALIZATION;
            }
            process_wrapper(|| plugin.reset());
        }

        self.current_buffer_config.store(Some(buffer_config));
        *self.buffer_manager.borrow_mut() = BufferManager::for_audio_io_layout(
            buffer_config.max_buffer_size as usize,
            audio_io_layout,
        );
        *self.io_buffers.borrow_mut() =
            IOBuffers::new(buffer_config.max_buffer_size as usize, audio_io_layout);
        *self.render_state.borrow_mut() = RenderState::new::<P>(audio_io_layout);

        0
    }

    fn uninitialize_inner(&self) -> OSStatus {
        if self.is_initialized.swap(false, Ordering::SeqCst) {
            self.current_buffer_config.store(None);
            self.plugin.lock().deactivate();
            *self.render_state.borrow_mut() =
                RenderState::new::<P>(self.current_audio_io_layout.load());
        }

        0
    }

    fn reset_inner(&self, scope: AudioUnitScope, element: AudioUnitElement) -> OSStatus {
        match scope {
            K_AUDIO_UNIT_SCOPE_GLOBAL if element == 0 => {
                process_wrapper(|| self.plugin.lock().reset());
                0
            }
            K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT
                if element < self.bus_count(scope) =>
            {
                process_wrapper(|| self.plugin.lock().reset());
                0
            }
            K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT => K_AUDIO_UNIT_ERR_INVALID_ELEMENT,
            _ => K_AUDIO_UNIT_ERR_INVALID_SCOPE,
        }
    }

    fn serialized_state(&self) -> Option<Vec<u8>> {
        permit_alloc(|| unsafe {
            state::serialize_json::<P>(
                self.params.clone(),
                state::make_params_iter(&self.param_by_hash, &self.param_id_to_hash),
            )
            .ok()
        })
    }

    fn set_state_inner(&self, state: &mut PluginState) -> bool {
        let success = permit_alloc(|| unsafe {
            state::deserialize_object::<P>(
                state,
                self.params.clone(),
                state::make_params_getter(&self.param_by_hash, &self.param_id_to_hash),
                self.current_buffer_config.load().as_ref(),
            )
        });
        if !success {
            return false;
        }

        if let Some(buffer_config) = self.current_buffer_config.load() {
            let audio_io_layout = self.current_audio_io_layout.load();
            let mut init_context = self.make_init_context();
            let mut plugin = self.plugin.lock();
            let reinitialized = permit_alloc(|| {
                plugin.initialize(&audio_io_layout, &buffer_config, &mut init_context)
            });
            if reinitialized {
                process_wrapper(|| plugin.reset());
            }

            if reinitialized {
                let task_posted = self.schedule_gui(Task::ParameterValuesChanged);
                nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
            }

            reinitialized
        } else {
            let task_posted = self.schedule_gui(Task::ParameterValuesChanged);
            nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
            true
        }
    }

    fn restore_defaults(&self) {
        for param_ptr in self.param_by_hash.values() {
            let default_normalized = unsafe { param_ptr.default_normalized_value() };
            unsafe { param_ptr.set_normalized_value(default_normalized) };
            if let Some(buffer_config) = self.current_buffer_config.load() {
                unsafe { param_ptr.update_smoother(buffer_config.sample_rate, true) };
            }
        }
        let task_posted = self.schedule_gui(Task::ParameterValuesChanged);
        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    fn push_parameter_change(&self, param_hash: u32, sample_offset: u32, normalized_value: f32) {
        let result = self.pending_parameter_changes.push(ScheduledParameterChange {
            param_hash,
            sample_offset,
            normalized_value,
        });
        if result.is_err() {
            nih_debug_assert_failure!("The AUv2 parameter change queue is full, dropping change");
        }
    }

    fn apply_parameter_change(&self, param_hash: u32, normalized_value: f32) -> OSStatus {
        let Some(param_ptr) = self.param_by_hash.get(&param_hash).copied() else {
            return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
        };

        if unsafe { param_ptr.set_normalized_value(normalized_value) } {
            if let Some(buffer_config) = self.current_buffer_config.load() {
                unsafe { param_ptr.update_smoother(buffer_config.sample_rate, false) };
            }

            let task_posted = self.schedule_gui(Task::ParameterValueChanged(
                param_hash,
                normalized_value,
            ));
            nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
        }

        0
    }

    fn set_plain_parameter(
        &self,
        param_id: AudioUnitParameterID,
        value: AudioUnitParameterValue,
        buffer_offset: u32,
    ) -> OSStatus {
        let Some(param_ptr) = self.param_by_hash.get(&param_id).copied() else {
            return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
        };
        let normalized_value = unsafe { param_ptr.preview_normalized(value) };

        if buffer_offset == 0 {
            self.apply_parameter_change(param_id, normalized_value)
        } else {
            self.push_parameter_change(param_id, buffer_offset, normalized_value);
            0
        }
    }

    fn queue_parameter_event(&self, event: &AudioUnitParameterEvent) -> OSStatus {
        if event.scope != K_AUDIO_UNIT_SCOPE_GLOBAL {
            return K_AUDIO_UNIT_ERR_INVALID_SCOPE;
        }

        match event.eventType {
            1 => {
                let immediate = unsafe { event.eventValues.immediate };
                self.set_plain_parameter(event.parameter, immediate.value, immediate.bufferOffset)
            }
            2 => {
                let ramp = unsafe { event.eventValues.ramp };
                let start_offset = ramp.startBufferOffset.max(0) as u32;
                let start_status =
                    self.set_plain_parameter(event.parameter, ramp.startValue, start_offset);
                if start_status != 0 {
                    return start_status;
                }

                if ramp.durationInFrames > 0 {
                    self.push_parameter_change(
                        event.parameter,
                        start_offset.saturating_add(ramp.durationInFrames),
                        unsafe {
                            self.param_by_hash[&event.parameter].preview_normalized(ramp.endValue)
                        },
                    );
                }

                0
            }
            _ => K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        }
    }

    fn queue_input_event(&self, event: PluginNoteEvent<P>) {
        let result = self.pending_input_events.push(event);
        if result.is_err() {
            nih_debug_assert_failure!("The AUv2 MIDI input queue is full, dropping event");
        }
    }

    fn queue_midi_message(&self, timing: u32, midi_data: &[u8]) -> OSStatus {
        if P::MIDI_INPUT == MidiConfig::None {
            return K_AUDIO_UNIT_ERR_INVALID_PROPERTY;
        }

        match crate::prelude::NoteEvent::from_midi(timing, midi_data) {
            Ok(event) => {
                self.queue_input_event(event);
                0
            }
            Err(_) => K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        }
    }

    fn build_transport(&self, time_stamp: *const AudioTimeStamp) -> Transport {
        let sample_rate = self.sample_rate.load() as f32;
        let mut transport = Transport::new(sample_rate);

        if !time_stamp.is_null() {
            let time_stamp = unsafe { &*time_stamp };
            if time_stamp.mFlags & K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID != 0 {
                transport.pos_samples = Some(time_stamp.mSampleTime.round() as i64);
            }
        }

        let Some(callbacks) = self.host_callbacks.load() else {
            return transport;
        };

        if let Some(proc) = callbacks.transportStateProc2 {
            let mut is_playing = 0;
            let mut is_recording = 0;
            let mut changed = 0;
            let mut current_sample = 0.0;
            let mut is_cycling = 0;
            let mut cycle_start = 0.0;
            let mut cycle_end = 0.0;
            if unsafe {
                proc(
                    callbacks.hostUserData,
                    &mut is_playing,
                    &mut is_recording,
                    &mut changed,
                    &mut current_sample,
                    &mut is_cycling,
                    &mut cycle_start,
                    &mut cycle_end,
                )
            } == 0
            {
                let _ = changed;
                transport.playing = is_playing != 0;
                transport.recording = is_recording != 0;
                transport.pos_samples = Some(current_sample.round() as i64);
                if is_cycling != 0 {
                    transport.loop_range_beats = Some((cycle_start, cycle_end));
                }
            }
        } else if let Some(proc) = callbacks.transportStateProc {
            let mut is_playing = 0;
            let mut changed = 0;
            let mut current_sample = 0.0;
            let mut is_cycling = 0;
            let mut cycle_start = 0.0;
            let mut cycle_end = 0.0;
            if unsafe {
                proc(
                    callbacks.hostUserData,
                    &mut is_playing,
                    &mut changed,
                    &mut current_sample,
                    &mut is_cycling,
                    &mut cycle_start,
                    &mut cycle_end,
                )
            } == 0
            {
                let _ = changed;
                transport.playing = is_playing != 0;
                transport.pos_samples = Some(current_sample.round() as i64);
                if is_cycling != 0 {
                    transport.loop_range_beats = Some((cycle_start, cycle_end));
                }
            }
        }

        if let Some(proc) = callbacks.beatAndTempoProc {
            let mut current_beat = 0.0;
            let mut current_tempo = 0.0;
            if unsafe { proc(callbacks.hostUserData, &mut current_beat, &mut current_tempo) } == 0 {
                transport.pos_beats = Some(current_beat);
                transport.tempo = Some(current_tempo);
            }
        }

        if let Some(proc) = callbacks.musicalTimeLocationProc {
            let mut samples_to_next_beat = 0;
            let mut numerator = 0.0;
            let mut denominator = 0;
            let mut measure_downbeat = 0.0;
            if unsafe {
                proc(
                    callbacks.hostUserData,
                    &mut samples_to_next_beat,
                    &mut numerator,
                    &mut denominator,
                    &mut measure_downbeat,
                )
            } == 0
            {
                let _ = samples_to_next_beat;
                transport.time_sig_numerator = Some(numerator.round() as i32);
                transport.time_sig_denominator = Some(denominator as i32);
                transport.bar_start_pos_beats = Some(measure_downbeat);
            }
        }

        transport
    }

    fn transport_for_block(&self, transport: &Transport, sample_offset: u32) -> Transport {
        let mut block_transport = Transport::new(transport.sample_rate);
        block_transport.playing = transport.playing;
        block_transport.recording = transport.recording;
        block_transport.preroll_active = transport.preroll_active;
        block_transport.tempo = transport.tempo;
        block_transport.time_sig_numerator = transport.time_sig_numerator;
        block_transport.time_sig_denominator = transport.time_sig_denominator;
        block_transport.loop_range_samples = transport.loop_range_samples;
        block_transport.loop_range_seconds = transport.loop_range_seconds;
        block_transport.loop_range_beats = transport.loop_range_beats;

        if let Some(pos_samples) = transport.pos_samples() {
            block_transport.pos_samples = Some(pos_samples + sample_offset as i64);
        }

        if let Some(pos_seconds) = transport.pos_seconds() {
            block_transport.pos_seconds =
                Some(pos_seconds + sample_offset as f64 / transport.sample_rate as f64);
        }

        if let (Some(pos_beats), Some(tempo)) = (transport.pos_beats(), transport.tempo) {
            block_transport.pos_beats = Some(
                pos_beats + sample_offset as f64 / transport.sample_rate as f64 / 60.0 * tempo,
            );
        }

        block_transport
    }

    fn render_cycle_key(
        in_time_stamp: *const AudioTimeStamp,
        in_number_frames: u32,
    ) -> RenderCycleKey {
        let sample_time_bits = if in_time_stamp.is_null() {
            None
        } else {
            let time_stamp = unsafe { &*in_time_stamp };
            (time_stamp.mFlags & K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID != 0)
                .then_some(time_stamp.mSampleTime.to_bits())
        };
        let host_time = if in_time_stamp.is_null() {
            0
        } else {
            unsafe { (*in_time_stamp).mHostTime }
        };

        RenderCycleKey {
            sample_time_bits,
            host_time,
            num_frames: in_number_frames,
        }
    }

    fn cycle_key_is_ambiguous(key: RenderCycleKey) -> bool {
        key.sample_time_bits.is_none() && key.host_time == 0
    }

    fn ensure_render_cycle_processed(
        &self,
        io_action_flags: *mut AudioUnitRenderActionFlags,
        in_time_stamp: *const AudioTimeStamp,
        in_output_bus_number: u32,
        in_number_frames: u32,
    ) -> Result<(), OSStatus> {
        let key = Self::render_cycle_key(in_time_stamp, in_number_frames);
        let output_bus_count = self.bus_count(K_AUDIO_UNIT_SCOPE_OUTPUT) as usize;
        {
            let render_state = self.render_state.borrow_mut();
            if render_state.last_cycle_key == Some(key)
                && (!Self::cycle_key_is_ambiguous(key)
                    || !render_state
                        .rendered_output_buses
                        .get(in_output_bus_number as usize)
                        .copied()
                        .unwrap_or(false))
            {
                return Ok(());
            }
        }

        let audio_io_layout = self.current_audio_io_layout.load();
        let mut io_buffers = self.io_buffers.borrow_mut();
        let mut buffer_manager = self.buffer_manager.borrow_mut();

        let has_main_input = audio_io_layout.main_input_channels.is_some();
        let has_main_output = audio_io_layout.main_output_channels.is_some();

        let main_output_pointers = if has_main_output {
            Some(io_buffers.output_bus_pointers(0, in_number_frames as usize)?)
        } else {
            None
        };
        for aux_output_idx in 0..io_buffers.aux_output_pointers.len() {
            let output_bus_idx = aux_output_idx + usize::from(has_main_output);
            io_buffers.aux_output_pointers[aux_output_idx] =
                Some(io_buffers.output_bus_pointers(output_bus_idx, in_number_frames as usize)?);
        }

        let main_input_pointers = if let Some(input_channels) = audio_io_layout.main_input_channels {
            Some(io_buffers.pull_input(
                0,
                self.input_callbacks.first().map(AtomicCell::load).flatten(),
                self.input_connections.first().map(AtomicCell::load).flatten(),
                io_action_flags,
                in_time_stamp,
                input_channels.get() as usize,
                in_number_frames,
            )?)
        } else {
            None
        };
        for aux_input_idx in 0..io_buffers.aux_input_pointers.len() {
            let input_bus_idx = aux_input_idx + usize::from(has_main_input);
            let input_channels = audio_io_layout.aux_input_ports[aux_input_idx].get() as usize;
            io_buffers.aux_input_pointers[aux_input_idx] = Some(io_buffers.pull_input(
                input_bus_idx as u32,
                self.input_callbacks
                    .get(input_bus_idx)
                    .map(AtomicCell::load)
                    .flatten(),
                self.input_connections
                    .get(input_bus_idx)
                    .map(AtomicCell::load)
                    .flatten(),
                io_action_flags,
                in_time_stamp,
                input_channels,
                in_number_frames,
            )?);
        }

        io_buffers.scratch_parameter_changes.clear();
        while let Some(change) = self.pending_parameter_changes.pop() {
            io_buffers.scratch_parameter_changes.push(change);
        }
        io_buffers
            .scratch_parameter_changes
            .sort_by_key(|change| change.sample_offset);

        io_buffers.scratch_input_events.clear();
        while let Some(mut event) = self.pending_input_events.pop() {
            let timing = clamp_input_event_timing(event.timing(), in_number_frames);
            set_event_timing(&mut event, timing);
            io_buffers.scratch_input_events.push(event);
        }
        io_buffers
            .scratch_input_events
            .sort_by_key(PluginNoteEvent::<P>::timing);

        io_buffers.scratch_output_events.clear();

        if !P::SAMPLE_ACCURATE_AUTOMATION {
            for change in io_buffers.scratch_parameter_changes.drain(..) {
                let _ = self.apply_parameter_change(change.param_hash, change.normalized_value);
            }
        }

        let mut sample_offset = 0_u32;
        let mut next_change_idx = 0_usize;
        let mut next_input_event_idx = 0_usize;
        let block_transport = self.build_transport(in_time_stamp);
        let has_changes = !io_buffers.scratch_parameter_changes.is_empty();
        let mut plugin = self.plugin.lock();

        while sample_offset < in_number_frames {
            while next_change_idx < io_buffers.scratch_parameter_changes.len()
                && io_buffers.scratch_parameter_changes[next_change_idx].sample_offset <= sample_offset
            {
                let change = io_buffers.scratch_parameter_changes[next_change_idx];
                let _ = self.apply_parameter_change(change.param_hash, change.normalized_value);
                next_change_idx += 1;
            }

            let next_change_sample = if P::SAMPLE_ACCURATE_AUTOMATION && has_changes {
                io_buffers
                    .scratch_parameter_changes
                    .get(next_change_idx)
                    .map(|change| change.sample_offset.min(in_number_frames))
                    .unwrap_or(in_number_frames)
            } else {
                in_number_frames
            };
            let block_len = next_change_sample.saturating_sub(sample_offset);
            if block_len == 0 {
                sample_offset = sample_offset.saturating_add(1);
                continue;
            }

            io_buffers.scratch_block_input_events.clear();
            while next_input_event_idx < io_buffers.scratch_input_events.len()
                && io_buffers.scratch_input_events[next_input_event_idx].timing()
                    < sample_offset + block_len
            {
                let mut event = io_buffers.scratch_input_events[next_input_event_idx].clone();
                if event.timing() >= sample_offset {
                    event.subtract_timing(sample_offset);
                    io_buffers.scratch_block_input_events.push(event);
                }
                next_input_event_idx += 1;
            }

            let buffers = unsafe {
                buffer_manager.create_buffers(sample_offset as usize, block_len as usize, |sources| {
                    *sources.main_input_channel_pointers = main_input_pointers;
                    *sources.main_output_channel_pointers = main_output_pointers;
                    sources
                        .aux_input_channel_pointers
                        .copy_from_slice(&io_buffers.aux_input_pointers);
                    sources
                        .aux_output_channel_pointers
                        .copy_from_slice(&io_buffers.aux_output_pointers);
                })
            };

            if !self.is_bypassed() {
                let transport = self.transport_for_block(&block_transport, sample_offset);
                let input_events_ptr = &io_buffers.scratch_block_input_events as *const Vec<_>;
                let output_events_ptr = &mut io_buffers.scratch_output_events as *mut Vec<_>;
                let mut process_context = self.make_process_context(
                    unsafe { &*input_events_ptr },
                    unsafe { &mut *output_events_ptr },
                    sample_offset,
                    transport,
                );
                let mut aux_buffers = AuxiliaryBuffers {
                    inputs: buffers.aux_inputs,
                    outputs: buffers.aux_outputs,
                };

                let status = process_wrapper(|| {
                    plugin.process(buffers.main_buffer, &mut aux_buffers, &mut process_context)
                });
                if let ProcessStatus::Error(err) = status {
                    nih_error!("The plugin returned an error while processing:");
                    nih_error!("{}", err);
                    return Err(K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT);
                }
            }

            sample_offset += block_len;
        }

        while next_change_idx < io_buffers.scratch_parameter_changes.len() {
            let change = io_buffers.scratch_parameter_changes[next_change_idx];
            let _ = self.apply_parameter_change(change.param_hash, change.normalized_value);
            next_change_idx += 1;
        }

        if P::MIDI_OUTPUT != MidiConfig::None {
            if let Some(callback) = self.midi_output_callback.load() {
                if let Some(proc) = callback.midiOutputCallback {
                    self.flush_output_events(
                        &mut io_buffers,
                        proc,
                        callback.userData,
                        in_time_stamp,
                        in_number_frames,
                    )?;
                }
            }
        }

        let mut render_state = self.render_state.borrow_mut();
        render_state.last_cycle_key = Some(key);
        render_state.rendered_output_buses.clear();
        render_state
            .rendered_output_buses
            .resize(output_bus_count.max(1), false);

        Ok(())
    }

    fn flush_output_events(
        &self,
        io_buffers: &mut IOBuffers<P>,
        callback: AUMIDIOutputCallback,
        user_data: *mut c_void,
        in_time_stamp: *const AudioTimeStamp,
        in_number_frames: u32,
    ) -> Result<(), OSStatus> {
        if io_buffers.scratch_output_events.is_empty() {
            return Ok(());
        }

        let packet_list_ptr =
            io_buffers.midi_packet_list_storage.as_mut_ptr() as *mut MIDIPacketList;
        let mut current_packet = unsafe { MIDIPacketListInit(packet_list_ptr) };

        for event in io_buffers.scratch_output_events.drain(..) {
            let timing = clamp_output_event_timing(event.timing(), in_number_frames) as u64;
            let Some(midi_result) = event.as_midi() else {
                continue;
            };

            match midi_result {
                MidiResult::Basic(bytes) => {
                    current_packet = self.add_midi_packet(
                        packet_list_ptr,
                        current_packet,
                        callback,
                        user_data,
                        in_time_stamp,
                        timing,
                        &bytes,
                    )?;
                }
                MidiResult::SysEx(buffer, length) => {
                    let padded_buffer = buffer.borrow();
                    let mut offset = 0;
                    while offset < length {
                        let end = (offset + MIDI_PACKET_CHUNK_SIZE).min(length);
                        current_packet = self.add_midi_packet(
                            packet_list_ptr,
                            current_packet,
                            callback,
                            user_data,
                            in_time_stamp,
                            timing,
                            &padded_buffer[offset..end],
                        )?;
                        offset = end;
                    }
                }
            }
        }

        if unsafe { (*packet_list_ptr).numPackets } > 0 {
            let status = unsafe { callback(user_data, in_time_stamp, 0, packet_list_ptr) };
            if status != 0 {
                return Err(status);
            }
        }

        Ok(())
    }

    fn add_midi_packet(
        &self,
        packet_list_ptr: *mut MIDIPacketList,
        current_packet: *mut MIDIPacket,
        callback: AUMIDIOutputCallback,
        user_data: *mut c_void,
        in_time_stamp: *const AudioTimeStamp,
        timing: u64,
        data: &[u8],
    ) -> Result<*mut MIDIPacket, OSStatus> {
        let mut next_packet = unsafe {
            MIDIPacketListAdd(
                packet_list_ptr,
                MIDI_PACKET_LIST_CAPACITY,
                current_packet,
                timing,
                data.len(),
                data.as_ptr(),
            )
        };
        if !next_packet.is_null() {
            return Ok(next_packet);
        }

        if unsafe { (*packet_list_ptr).numPackets } > 0 {
            let status = unsafe { callback(user_data, in_time_stamp, 0, packet_list_ptr) };
            if status != 0 {
                return Err(status);
            }
        }

        let current_packet = unsafe { MIDIPacketListInit(packet_list_ptr) };
        next_packet = unsafe {
            MIDIPacketListAdd(
                packet_list_ptr,
                MIDI_PACKET_LIST_CAPACITY,
                current_packet,
                timing,
                data.len(),
                data.as_ptr(),
            )
        };
        if next_packet.is_null() {
            Err(K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT)
        } else {
            Ok(next_packet)
        }
    }

    fn render_inner(
        &self,
        io_action_flags: *mut AudioUnitRenderActionFlags,
        in_time_stamp: *const AudioTimeStamp,
        in_output_bus_number: u32,
        in_number_frames: u32,
        io_data: *mut AudioBufferList,
    ) -> OSStatus {
        if !self.is_initialized.load(Ordering::Relaxed) {
            return K_AUDIO_UNIT_ERR_UNINITIALIZED;
        }
        let output_bus_count = self.bus_count(K_AUDIO_UNIT_SCOPE_OUTPUT);
        if output_bus_count > 0 && in_output_bus_number >= output_bus_count {
            return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
        }
        if in_number_frames > self.maximum_frames_per_slice.load(Ordering::Relaxed) {
            return K_AUDIO_UNIT_ERR_TOO_MANY_FRAMES_TO_PROCESS;
        }
        let process_status = self.ensure_render_cycle_processed(
            io_action_flags,
            in_time_stamp,
            in_output_bus_number,
            in_number_frames,
        );
        if let Err(status) = process_status {
            return status;
        }

        if output_bus_count == 0 {
            return 0;
        }

        let output_channels = self
            .bus_channels(K_AUDIO_UNIT_SCOPE_OUTPUT, in_output_bus_number)
            .unwrap_or(0) as usize;
        if let Err(status) = self.io_buffers.borrow().copy_output_bus_to_host(
            in_output_bus_number as usize,
            io_data,
            output_channels,
            in_number_frames as usize,
        ) {
            return status;
        }

        self.render_state.borrow_mut().mark_rendered(in_output_bus_number as usize);
        if let Some(io_action_flags) = unsafe { io_action_flags.as_mut() } {
            let audio_io_layout = self.current_audio_io_layout.load();
            let has_matching_input = if audio_io_layout.main_output_channels.is_some()
                && in_output_bus_number == 0
            {
                audio_io_layout.main_input_channels.is_some()
            } else {
                false
            };
            if self.is_bypassed() && !has_matching_input {
                *io_action_flags |= K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE;
            }
        }

        0
    }
}

impl<P: Auv2Plugin> InitContext<P> for WrapperInitContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        Auv2
    }

    fn execute(&self, task: P::BackgroundTask) {
        (self.wrapper.task_executor.lock())(task);
    }

    fn set_latency_samples(&self, samples: u32) {
        self.wrapper.set_latency_samples(samples);
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

impl<P: Auv2Plugin> ProcessContext<P> for WrapperProcessContext<'_, P> {
    fn plugin_api(&self) -> PluginApi {
        Auv2
    }

    fn execute_background(&self, task: P::BackgroundTask) {
        let task_posted = self.wrapper.schedule_background(Task::PluginTask(task));
        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    fn execute_gui(&self, task: P::BackgroundTask) {
        let task_posted = self.wrapper.schedule_gui(Task::PluginTask(task));
        nih_debug_assert!(task_posted, "The task queue is full, dropping task...");
    }

    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<PluginNoteEvent<P>> {
        if self.input_event_idx < self.input_events.len() {
            let event = self.input_events[self.input_event_idx].clone();
            self.input_event_idx += 1;
            Some(event)
        } else {
            None
        }
    }

    fn send_event(&mut self, mut event: PluginNoteEvent<P>) {
        if P::MIDI_OUTPUT == MidiConfig::None {
            return;
        }

        add_event_timing(&mut event, self.event_timing_offset);
        self.output_events.push(event);
    }

    fn set_latency_samples(&self, samples: u32) {
        self.wrapper.set_latency_samples(samples);
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

impl<P: Auv2Plugin> MainThreadExecutor<Task<P>> for Wrapper<P> {
    fn execute(&self, task: Task<P>, _is_gui_thread: bool) {
        match task {
            Task::PluginTask(task) => (self.task_executor.lock())(task),
            Task::ParameterValuesChanged => {
                if self.open_editor_count.load(Ordering::Relaxed) > 0 {
                    if let Some(editor) = self.editor.borrow().as_ref() {
                        editor.lock().param_values_changed();
                    }
                }
            }
            Task::ParameterValueChanged(param_hash, normalized_value) => {
                if self.open_editor_count.load(Ordering::Relaxed) > 0 {
                    if let Some(editor) = self.editor.borrow().as_ref() {
                        if let Some(param_id) = self.param_id_by_hash.get(&param_hash) {
                            editor
                                .lock()
                                .param_value_changed(param_id, normalized_value);
                        }
                    }
                }
            }
        }
    }
}

impl<P: Auv2Plugin> GuiContext for WrapperGuiContext<P> {
    fn plugin_api(&self) -> PluginApi {
        Auv2
    }

    fn request_resize(&self) -> bool {
        let cocoa_view = self.wrapper.cocoa_view.load() as *mut Object;
        if cocoa_view.is_null() {
            return false;
        }

        let editor_guard = self.wrapper.editor.borrow();
        let Some(editor) = editor_guard.as_ref() else {
            return false;
        };
        let (width, height) = editor.lock().size();

        unsafe { resize_cocoa_view(cocoa_view, width as f64, height as f64) }
    }

    unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
        let Some(hash) = self.wrapper.param_ptr_to_hash.get(&param).copied() else {
            return;
        };
        let audio_unit = self.wrapper.component_instance.load() as AudioUnit;
        if audio_unit.is_null() {
            return;
        }
        let event = AudioUnitEvent {
            mEventType: K_AUDIO_UNIT_EVENT_BEGIN_PARAMETER_CHANGE_GESTURE,
            mArgument: AudioUnitEventArgument {
                mParameter: AudioUnitParameter {
                    mAudioUnit: audio_unit,
                    mParameterID: hash,
                    mScope: K_AUDIO_UNIT_SCOPE_GLOBAL,
                    mElement: 0,
                },
            },
        };

        unsafe {
            let _ = AUEventListenerNotify(ptr::null_mut(), ptr::null_mut(), &event);
        }
    }

    unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
        let Some(hash) = self.wrapper.param_ptr_to_hash.get(&param).copied() else {
            return;
        };
        let plain_value = unsafe { param.preview_plain(normalized) };
        let audio_unit = self.wrapper.component_instance.load() as AudioUnit;
        if audio_unit.is_null() {
            let _ = self.wrapper.apply_parameter_change(hash, normalized);
            return;
        }
        let parameter = AudioUnitParameter {
            mAudioUnit: audio_unit,
            mParameterID: hash,
            mScope: K_AUDIO_UNIT_SCOPE_GLOBAL,
            mElement: 0,
        };

        let status = unsafe {
            AUParameterSet(
                ptr::null_mut(),
                ptr::null_mut(),
                &parameter,
                plain_value,
                0,
            )
        };
        if status != 0 {
            let _ = self.wrapper.apply_parameter_change(hash, normalized);
        }
    }

    unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
        let Some(hash) = self.wrapper.param_ptr_to_hash.get(&param).copied() else {
            return;
        };
        let audio_unit = self.wrapper.component_instance.load() as AudioUnit;
        if audio_unit.is_null() {
            return;
        }
        let event = AudioUnitEvent {
            mEventType: K_AUDIO_UNIT_EVENT_END_PARAMETER_CHANGE_GESTURE,
            mArgument: AudioUnitEventArgument {
                mParameter: AudioUnitParameter {
                    mAudioUnit: audio_unit,
                    mParameterID: hash,
                    mScope: K_AUDIO_UNIT_SCOPE_GLOBAL,
                    mElement: 0,
                },
            },
        };

        unsafe {
            let _ = AUEventListenerNotify(ptr::null_mut(), ptr::null_mut(), &event);
        }
    }

    fn get_state(&self) -> PluginState {
        self.wrapper.get_state_object()
    }

    fn set_state(&self, state: PluginState) {
        self.wrapper.set_state_object_from_gui(state);
    }
}

impl<P: Auv2Plugin> IOBuffers<P> {
    fn new(max_buffer_size: usize, audio_io_layout: AudioIOLayout) -> Self {
        Self {
            input_busses: Wrapper::<P>::bus_channel_counts_for_layout(
                audio_io_layout,
                K_AUDIO_UNIT_SCOPE_INPUT,
            )
            .into_iter()
            .map(|channels| BusIOBuffers {
                storage: (0..channels).map(|_| vec![0.0; max_buffer_size]).collect(),
                channel_pointers: vec![ptr::null_mut(); channels as usize],
                buffer_list: FixedAudioBufferList {
                    mNumberBuffers: 0,
                    mBuffers: [AudioBuffer {
                        mNumberChannels: 1,
                        mDataByteSize: 0,
                        mData: ptr::null_mut(),
                    }; MAX_AUDIO_CHANNELS],
                },
            })
            .collect(),
            output_busses: Wrapper::<P>::bus_channel_counts_for_layout(
                audio_io_layout,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
            )
            .into_iter()
            .map(|channels| OutputBusBuffers {
                storage: (0..channels).map(|_| vec![0.0; max_buffer_size]).collect(),
                channel_pointers: vec![ptr::null_mut(); channels as usize],
            })
            .collect(),
            aux_input_pointers: vec![None; audio_io_layout.aux_input_ports.len()],
            aux_output_pointers: vec![None; audio_io_layout.aux_output_ports.len()],
            scratch_parameter_changes: Vec::with_capacity(PARAMETER_QUEUE_CAPACITY),
            scratch_input_events: Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY),
            scratch_block_input_events: Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY),
            scratch_output_events: Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY),
            midi_packet_list_storage: vec![0; MIDI_PACKET_LIST_CAPACITY].into_boxed_slice(),
        }
    }

    fn output_bus_pointers(
        &mut self,
        bus_idx: usize,
        num_frames: usize,
    ) -> Result<ChannelPointers, OSStatus> {
        let Some(output_bus) = self.output_busses.get_mut(bus_idx) else {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        };
        if output_bus.channel_pointers.is_empty() {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        }

        for (channel_storage, channel_ptr) in output_bus
            .storage
            .iter_mut()
            .zip(output_bus.channel_pointers.iter_mut())
        {
            *channel_ptr = channel_storage.as_mut_ptr();
            channel_storage[..num_frames].fill(0.0);
        }

        Ok(ChannelPointers {
            ptrs: NonNull::new(output_bus.channel_pointers.as_mut_ptr())
                .expect("Output pointers vector should never be empty"),
            num_channels: output_bus.channel_pointers.len(),
        })
    }

    fn copy_output_bus_to_host(
        &self,
        bus_idx: usize,
        io_data: *mut AudioBufferList,
        output_channels: usize,
        num_frames: usize,
    ) -> Result<(), OSStatus> {
        if output_channels == 0 {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        }

        let io_data = unsafe { io_data.as_mut() }.ok_or(K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE)?;
        let Some(output_bus) = self.output_busses.get(bus_idx) else {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        };
        let buffers = unsafe {
            slice::from_raw_parts_mut(
                io_data.mBuffers.as_mut_ptr(),
                io_data.mNumberBuffers as usize,
            )
        };

        match io_data.mNumberBuffers as usize {
            count if count >= output_channels => {
                for channel_idx in 0..output_channels {
                    let buffer = &mut buffers[channel_idx];
                    let required_size = (num_frames * mem::size_of::<f32>()) as u32;
                    if buffer.mData.is_null() {
                        buffer.mNumberChannels = 1;
                        buffer.mDataByteSize = required_size;
                        buffer.mData = output_bus.storage[channel_idx].as_ptr() as *mut c_void;
                        continue;
                    }

                    if buffer.mDataByteSize < required_size {
                        return Err(K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE);
                    }

                    unsafe {
                        slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), num_frames)
                            .copy_from_slice(&output_bus.storage[channel_idx][..num_frames]);
                    }
                }
            }
            1 => {
                let buffer = &mut buffers[0];
                let required_size =
                    (num_frames * output_channels * mem::size_of::<f32>()) as u32;
                if buffer.mData.is_null()
                    || buffer.mNumberChannels != output_channels as u32
                    || buffer.mDataByteSize < required_size
                {
                    return Err(K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE);
                }

                let interleaved =
                    unsafe { slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), num_frames * output_channels) };
                for frame_idx in 0..num_frames {
                    for channel_idx in 0..output_channels {
                        interleaved[(frame_idx * output_channels) + channel_idx] =
                            output_bus.storage[channel_idx][frame_idx];
                    }
                }
            }
            _ => return Err(K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE),
        }

        Ok(())
    }

    fn pull_input(
        &mut self,
        bus_idx: u32,
        input_callback: Option<AURenderCallbackStruct>,
        input_connection: Option<AudioUnitConnection>,
        io_action_flags: *mut AudioUnitRenderActionFlags,
        in_time_stamp: *const AudioTimeStamp,
        input_channels: usize,
        in_number_frames: u32,
    ) -> Result<ChannelPointers, OSStatus> {
        let Some(input_bus) = self.input_busses.get_mut(bus_idx as usize) else {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        };
        if input_channels == 0 {
            return Err(K_AUDIO_UNIT_ERR_INVALID_ELEMENT);
        }

        input_bus.buffer_list.mNumberBuffers = input_channels as u32;
        for channel_idx in 0..input_channels {
            let storage = input_bus
                .storage
                .get_mut(channel_idx)
                .ok_or(K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE)?;
            input_bus.channel_pointers[channel_idx] = storage.as_mut_ptr();
            input_bus.buffer_list.mBuffers[channel_idx] = AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: in_number_frames * mem::size_of::<f32>() as u32,
                mData: storage.as_mut_ptr().cast(),
            };
        }

        let status = if let Some(connection) = input_connection {
            if connection.sourceAudioUnit.is_null() {
                K_AUDIO_UNIT_ERR_NO_CONNECTION
            } else {
                unsafe {
                    AudioUnitRender(
                        connection.sourceAudioUnit,
                        io_action_flags,
                        in_time_stamp,
                        connection.sourceOutputNumber,
                        in_number_frames,
                        input_bus.buffer_list.as_audio_buffer_list_mut_ptr(),
                    )
                }
            }
        } else if let Some(callback) = input_callback {
            match callback.inputProc {
                Some(proc) => unsafe {
                    proc(
                        callback.inputProcRefCon,
                        io_action_flags,
                        in_time_stamp,
                        bus_idx,
                        in_number_frames,
                        input_bus.buffer_list.as_audio_buffer_list_mut_ptr(),
                    )
                },
                None => K_AUDIO_UNIT_ERR_NO_CONNECTION,
            }
        } else {
            0
        };
        if status != 0 {
            return Err(status);
        }

        if input_callback.is_none() && input_connection.is_none() {
            for storage in &mut input_bus.storage {
                storage[..in_number_frames as usize].fill(0.0);
            }
        }

        Ok(ChannelPointers {
            ptrs: NonNull::new(input_bus.channel_pointers.as_mut_ptr())
                .expect("Input pointers vector should never be empty"),
            num_channels: input_channels,
        })
    }
}

impl RenderState {
    fn new<P: Auv2Plugin>(audio_io_layout: AudioIOLayout) -> Self {
        Self {
            last_cycle_key: None,
            rendered_output_buses: vec![
                false;
                Wrapper::<P>::bus_count_for_layout(audio_io_layout, K_AUDIO_UNIT_SCOPE_OUTPUT)
                    as usize
            ],
        }
    }

    fn mark_rendered(&mut self, bus_idx: usize) {
        if let Some(rendered) = self.rendered_output_buses.get_mut(bus_idx) {
            *rendered = true;
        }
    }
}

impl FixedAudioBufferList {
    fn as_audio_buffer_list_mut_ptr(&mut self) -> *mut AudioBufferList {
        self as *mut _ as *mut AudioBufferList
    }
}

unsafe extern "C" fn open<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    instance: AudioComponentInstance,
) -> OSStatus {
    if self_ptr.is_null() {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }

    register_audio_unit_instance::<P>(self_ptr, instance);
    0
}

unsafe extern "C" fn close<P: Auv2Plugin>(self_ptr: *mut c_void) -> OSStatus {
    if self_ptr.is_null() {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }

    unregister_audio_unit_instance::<P>(self_ptr);
    drop(unsafe { Arc::from_raw(self_ptr.cast::<Wrapper<P>>()) });
    0
}

unsafe extern "C" fn lookup<P: Auv2Plugin>(selector: i16) -> AudioComponentMethod {
    match selector {
        K_AUDIO_UNIT_INITIALIZE_SELECT => initialize::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_UNINITIALIZE_SELECT => uninitialize::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_GET_PROPERTY_INFO_SELECT => get_property_info::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_GET_PROPERTY_SELECT => get_property::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_SET_PROPERTY_SELECT => set_property::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_GET_PARAMETER_SELECT => get_parameter::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_SET_PARAMETER_SELECT => set_parameter::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_ADD_PROPERTY_LISTENER_SELECT => {
            add_property_listener::<P> as AudioComponentMethod
        }
        K_AUDIO_UNIT_REMOVE_PROPERTY_LISTENER_SELECT => {
            remove_property_listener::<P> as AudioComponentMethod
        }
        K_AUDIO_UNIT_REMOVE_PROPERTY_LISTENER_WITH_USER_DATA_SELECT => {
            remove_property_listener_with_user_data::<P> as AudioComponentMethod
        }
        K_AUDIO_UNIT_ADD_RENDER_NOTIFY_SELECT => add_render_notify::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_REMOVE_RENDER_NOTIFY_SELECT => {
            remove_render_notify::<P> as AudioComponentMethod
        }
        K_AUDIO_UNIT_SCHEDULE_PARAMETERS_SELECT => {
            schedule_parameters::<P> as AudioComponentMethod
        }
        K_AUDIO_UNIT_RENDER_SELECT => render::<P> as AudioComponentMethod,
        K_AUDIO_UNIT_RESET_SELECT => reset::<P> as AudioComponentMethod,
        K_MUSIC_DEVICE_MIDI_EVENT_SELECT => music_device_midi_event::<P> as AudioComponentMethod,
        K_MUSIC_DEVICE_SYS_EX_SELECT => music_device_sysex::<P> as AudioComponentMethod,
        _ => ptr::null(),
    }
}

unsafe extern "C" fn add_property_listener<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    listener: Option<AudioUnitPropertyListenerProc>,
    user_data: *mut c_void,
) -> OSStatus {
    let Some(proc) = listener else {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    };

    wrapper_from_raw::<P>(self_ptr)
        .property_listeners
        .lock()
        .push(PropertyListener {
            property_id,
            proc,
            user_data: user_data as usize,
        });

    0
}

unsafe extern "C" fn remove_property_listener<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    listener: Option<AudioUnitPropertyListenerProc>,
) -> OSStatus {
    let Some(proc) = listener else {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    };
    let proc_addr = proc as usize;

    wrapper_from_raw::<P>(self_ptr)
        .property_listeners
        .lock()
        .retain(|listener| {
            listener.property_id != property_id || listener.proc as usize != proc_addr
        });

    0
}

unsafe extern "C" fn remove_property_listener_with_user_data<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    listener: Option<AudioUnitPropertyListenerProc>,
    user_data: *mut c_void,
) -> OSStatus {
    let Some(proc) = listener else {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    };
    let proc_addr = proc as usize;
    let user_data = user_data as usize;

    wrapper_from_raw::<P>(self_ptr)
        .property_listeners
        .lock()
        .retain(|listener| {
            listener.property_id != property_id
                || listener.proc as usize != proc_addr
                || listener.user_data != user_data
        });

    0
}

unsafe extern "C" fn add_render_notify<P: Auv2Plugin>(
    _self_ptr: *mut c_void,
    _callback: Option<AURenderCallback>,
    _user_data: *mut c_void,
) -> OSStatus {
    0
}

unsafe extern "C" fn remove_render_notify<P: Auv2Plugin>(
    _self_ptr: *mut c_void,
    _callback: Option<AURenderCallback>,
    _user_data: *mut c_void,
) -> OSStatus {
    0
}

unsafe extern "C" fn initialize<P: Auv2Plugin>(self_ptr: *mut c_void) -> OSStatus {
    wrapper_from_raw::<P>(self_ptr).initialize_inner()
}

unsafe extern "C" fn uninitialize<P: Auv2Plugin>(self_ptr: *mut c_void) -> OSStatus {
    wrapper_from_raw::<P>(self_ptr).uninitialize_inner()
}

unsafe extern "C" fn get_property_info<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out_data_size: *mut u32,
    out_writable: *mut Boolean,
) -> OSStatus {
    let wrapper = wrapper_from_raw::<P>(self_ptr);
    let data_size = match property_id {
        K_AUDIO_UNIT_PROPERTY_CLASS_INFO if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            mem::size_of::<CFDictionaryRef>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            mem::size_of::<f64>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            (wrapper.param_hashes.len() * mem::size_of::<AudioUnitParameterID>()) as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && wrapper.param_by_hash.contains_key(&element) =>
        {
            mem::size_of::<AudioUnitParameterInfo>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            mem::size_of::<AudioStreamBasicDescription>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT
            if matches!(
                scope,
                K_AUDIO_UNIT_SCOPE_GLOBAL | K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT
            ) =>
        {
            mem::size_of::<u32>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_LATENCY if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            mem::size_of::<f64>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            (wrapper.supported_channel_infos.len() * mem::size_of::<AUChannelInfo>()) as u32
        }
        K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<u32>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && wrapper
                .param_by_hash
                .get(&element)
                .is_some_and(|param| unsafe { param.step_count() }.is_some()) =>
        {
            mem::size_of::<CFArrayRef>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_TAIL_TIME if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            mem::size_of::<f64>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<u32>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            mem::size_of::<AudioUnitConnection>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            mem::size_of::<AURenderCallbackStruct>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<HostCallbackInfo>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<u32>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_COCOA_UI
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && wrapper.editor.borrow().is_some() =>
        {
            mem::size_of::<AudioUnitCocoaViewInfo>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<AUPreset>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            mem::size_of::<AUPreset>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            mem::size_of::<AudioUnitParameterStringFromValue>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            mem::size_of::<AudioUnitParameterValueFromString>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_ID_NAME
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && wrapper.param_by_hash.contains_key(&element) =>
        {
            mem::size_of::<AudioUnitParameterIDName>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && P::MIDI_OUTPUT != MidiConfig::None =>
        {
            mem::size_of::<CFArrayRef>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && P::MIDI_OUTPUT != MidiConfig::None =>
        {
            mem::size_of::<AUMIDIOutputCallbackStruct>() as u32
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        K_AUDIO_UNIT_PROPERTY_PARAMETER_ID_NAME => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET => return K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
        K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET => return K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
        _ => return K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
    };

    unsafe {
        if let Some(out_data_size) = out_data_size.as_mut() {
            *out_data_size = data_size;
        }
        if let Some(out_writable) = out_writable.as_mut() {
            *out_writable = match property_id {
                K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST
                | K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO
                | K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT
                | K_AUDIO_UNIT_PROPERTY_LATENCY
                | K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS
                | K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS
                | K_AUDIO_UNIT_PROPERTY_TAIL_TIME
                | K_AUDIO_UNIT_PROPERTY_COCOA_UI
                | K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO
                | K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE
                | K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING
                | K_AUDIO_UNIT_PROPERTY_PARAMETER_ID_NAME
                | K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET
                | K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET => 0,
                _ => 1,
            };
        }
    }

    0
}

unsafe extern "C" fn get_property<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out_data: *mut c_void,
    io_data_size: *mut u32,
) -> OSStatus {
    let wrapper = wrapper_from_raw::<P>(self_ptr);
    if out_data.is_null() {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }

    match property_id {
        K_AUDIO_UNIT_PROPERTY_CLASS_INFO if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            let Some(serialized_state) = wrapper.serialized_state() else {
                return K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT;
            };
            let dictionary = class_info_dictionary::<P>(&serialized_state);
            let dictionary_ref = dictionary.as_concrete_TypeRef();
            mem::forget(dictionary);
            unsafe {
                *(out_data as *mut CFDictionaryRef) = dictionary_ref;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<CFDictionaryRef>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            unsafe {
                *(out_data as *mut f64) = wrapper.sample_rate.load();
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<f64>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            unsafe {
                ptr::copy_nonoverlapping(
                    wrapper.param_hashes.as_ptr(),
                    out_data as *mut AudioUnitParameterID,
                    wrapper.param_hashes.len(),
                );
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size =
                        (wrapper.param_hashes.len() * mem::size_of::<AudioUnitParameterID>())
                            as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            let Some(param_ptr) = wrapper.param_by_hash.get(&element).copied() else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };
            unsafe {
                *(out_data as *mut AudioUnitParameterInfo) = parameter_info(param_ptr);
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitParameterInfo>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            let Ok(stream_format) = wrapper.current_stream_format(scope, element) else {
                return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
            };
            unsafe {
                *(out_data as *mut AudioStreamBasicDescription) = stream_format;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioStreamBasicDescription>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_ELEMENT_COUNT
            if matches!(
                scope,
                K_AUDIO_UNIT_SCOPE_GLOBAL | K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT
            ) =>
        {
            let count = match scope {
                K_AUDIO_UNIT_SCOPE_GLOBAL => 1,
                K_AUDIO_UNIT_SCOPE_INPUT => wrapper.bus_count(scope),
                K_AUDIO_UNIT_SCOPE_OUTPUT => wrapper.bus_count(scope),
                _ => unreachable!(),
            };
            unsafe {
                *(out_data as *mut u32) = count;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<u32>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_LATENCY if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            let latency = wrapper.current_latency.load(Ordering::Relaxed) as f64
                / wrapper.sample_rate.load().max(1.0);
            unsafe {
                *(out_data as *mut f64) = latency;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<f64>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_SUPPORTED_NUM_CHANNELS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            unsafe {
                ptr::copy_nonoverlapping(
                    wrapper.supported_channel_infos.as_ptr(),
                    out_data as *mut AUChannelInfo,
                    wrapper.supported_channel_infos.len(),
                );
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size =
                        (wrapper.supported_channel_infos.len() * mem::size_of::<AUChannelInfo>())
                            as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            unsafe {
                *(out_data as *mut u32) =
                    wrapper.maximum_frames_per_slice.load(Ordering::Relaxed);
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<u32>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            let Some(param_ptr) = wrapper.param_by_hash.get(&element).copied() else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };
            let Some(step_count) = (unsafe { param_ptr.step_count() }) else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };

            let strings: Vec<_> = (0..=step_count)
                .map(|index| {
                    let normalized = if step_count == 0 {
                        0.0
                    } else {
                        index as f32 / step_count as f32
                    };
                    CFString::new(&unsafe {
                        param_ptr.normalized_value_to_string(normalized, false)
                    })
                })
                .collect();
            let array = CFArray::from_CFTypes(&strings);
            let array_ref = array.as_concrete_TypeRef();
            mem::forget(array);

            unsafe {
                *(out_data as *mut CFArrayRef) = array_ref;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<CFArrayRef>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_TAIL_TIME if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            unsafe {
                *(out_data as *mut f64) = 0.0;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<f64>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            unsafe {
                *(out_data as *mut u32) = wrapper.is_bypassed() as u32;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<u32>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            let connection = wrapper
                .input_connections
                .get(element as usize)
                .map(AtomicCell::load)
                .flatten()
                .unwrap_or(AudioUnitConnection {
                    sourceAudioUnit: ptr::null_mut(),
                    sourceOutputNumber: 0,
                    destInputNumber: element,
                });
            unsafe {
                *(out_data as *mut AudioUnitConnection) = connection;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitConnection>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            let callback = wrapper
                .input_callbacks
                .get(element as usize)
                .map(AtomicCell::load)
                .flatten()
                .unwrap_or(AURenderCallbackStruct {
                    inputProc: None,
                    inputProcRefCon: ptr::null_mut(),
                });
            unsafe {
                *(out_data as *mut AURenderCallbackStruct) = callback;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AURenderCallbackStruct>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            unsafe {
                *(out_data as *mut u32) =
                    wrapper.in_place_processing.load(Ordering::Relaxed) as u32;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<u32>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_COCOA_UI
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && wrapper.editor.borrow().is_some() =>
        {
            let factory_class = cocoa_factory_class::<P>();
            let bundle_url = bundle_url_for_class(factory_class);
            if bundle_url.is_null() {
                return K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT;
            }

            let class_name = CFString::new(factory_class.name());
            let info = AudioUnitCocoaViewInfo {
                mCocoaAUViewBundleLocation: bundle_url,
                mCocoaAUViewClass: [class_name.as_concrete_TypeRef()],
            };
            mem::forget(class_name);

            unsafe {
                *(out_data as *mut AudioUnitCocoaViewInfo) = info;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitCocoaViewInfo>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            let preset_name = CFString::new("Default");
            let preset = AUPreset {
                presetNumber: 0,
                presetName: preset_name.as_concrete_TypeRef(),
            };
            mem::forget(preset_name);

            unsafe {
                *(out_data as *mut AUPreset) = preset;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AUPreset>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            let preset_name = CFString::new("Default");
            let preset = AUPreset {
                presetNumber: 0,
                presetName: preset_name.as_concrete_TypeRef(),
            };
            mem::forget(preset_name);

            unsafe {
                *(out_data as *mut AUPreset) = preset;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AUPreset>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            if out_data.is_null() || io_data_size.is_null() {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let query = unsafe { &mut *(out_data as *mut AudioUnitParameterStringFromValue) };
            let Some(param_ptr) = wrapper.param_by_hash.get(&query.inParamID).copied() else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };
            let value = if query.inValue.is_null() {
                unsafe { param_ptr.modulated_normalized_value() }
            } else {
                unsafe { *query.inValue }
            };
            let string = CFString::new(&unsafe {
                param_ptr.normalized_value_to_string(value, !param_ptr.step_count().is_some())
            });
            query.outString = string.as_concrete_TypeRef();
            mem::forget(string);

            unsafe {
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitParameterStringFromValue>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            if out_data.is_null() || io_data_size.is_null() {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let query = unsafe { &mut *(out_data as *mut AudioUnitParameterValueFromString) };
            let Some(param_ptr) = wrapper.param_by_hash.get(&query.inParamID).copied() else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };
            let string = unsafe { CFString::wrap_under_get_rule(query.inString) };
            let rust_string = string.to_string();
            let Some(normalized) = (unsafe { param_ptr.string_to_normalized_value(&rust_string) }) else {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            };
            let plain_value = unsafe { param_ptr.preview_plain(normalized) };
            query.outValue = plain_value;

            unsafe {
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitParameterValueFromString>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_PARAMETER_ID_NAME
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL =>
        {
            let Some(param_ptr) = wrapper.param_by_hash.get(&element).copied() else {
                return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
            };
            let name_info = unsafe { &mut *(out_data as *mut AudioUnitParameterIDName) };
            let param_name = unsafe { param_ptr.name() };
            let desired_length = name_info.inDesiredLength;
            let truncated_name = if desired_length >= 0 && (param_name.len() as i32) > desired_length {
                &param_name[..desired_length as usize]
            } else {
                param_name
            };
            let cf_name = CFString::new(truncated_name);
            name_info.outName = cf_name.as_concrete_TypeRef();
            mem::forget(cf_name);

            unsafe {
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AudioUnitParameterIDName>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK_INFO
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && P::MIDI_OUTPUT != MidiConfig::None =>
        {
            let names = [CFString::new("MIDI Output")];
            let array = CFArray::from_CFTypes(&names);
            let array_ref = array.as_concrete_TypeRef();
            mem::forget(array);

            unsafe {
                *(out_data as *mut CFArrayRef) = array_ref;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<CFArrayRef>() as u32;
                }
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && P::MIDI_OUTPUT != MidiConfig::None =>
        {
            let callback = wrapper
                .midi_output_callback
                .load()
                .unwrap_or(AUMIDIOutputCallbackStruct {
                    midiOutputCallback: None,
                    userData: ptr::null_mut(),
                });
            unsafe {
                *(out_data as *mut AUMIDIOutputCallbackStruct) = callback;
                if let Some(io_data_size) = io_data_size.as_mut() {
                    *io_data_size = mem::size_of::<AUMIDIOutputCallbackStruct>() as u32;
                }
            }
            0
        }
        _ => K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
    }
}

unsafe extern "C" fn set_property<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    property_id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    in_data: *const c_void,
    in_data_size: u32,
) -> OSStatus {
    let wrapper = wrapper_from_raw::<P>(self_ptr);

    match property_id {
        K_AUDIO_UNIT_PROPERTY_CLASS_INFO if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 => {
            if in_data.is_null() || in_data_size != mem::size_of::<CFDictionaryRef>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }

            let dictionary_ref = unsafe { *(in_data as *const CFDictionaryRef) };
            if dictionary_ref.is_null() {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let dictionary = unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(dictionary_ref) };
            let Some(state) = deserialize_class_info(&dictionary) else {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            };

            let mut state = state;
            if wrapper.set_state_inner(&mut state) {
                0
            } else {
                K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE
            }
        }
        K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<f64>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            if wrapper.is_initialized.load(Ordering::Relaxed) {
                return K_AUDIO_UNIT_ERR_INITIALIZED;
            }

            let sample_rate = unsafe { *(in_data as *const f64) };
            if sample_rate <= 0.0 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }

            wrapper.sample_rate.store(sample_rate);
            0
        }
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT
            if matches!(scope, K_AUDIO_UNIT_SCOPE_INPUT | K_AUDIO_UNIT_SCOPE_OUTPUT)
                && wrapper.bus_channels(scope, element).is_some() =>
        {
            if in_data.is_null()
                || in_data_size != mem::size_of::<AudioStreamBasicDescription>() as u32
            {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }

            let stream_format = unsafe { &*(in_data as *const AudioStreamBasicDescription) };
            wrapper.set_stream_format(scope, element, stream_format)
        }
        K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<u32>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            if wrapper.is_initialized.load(Ordering::Relaxed) {
                return K_AUDIO_UNIT_ERR_INITIALIZED;
            }

            let maximum_frames = unsafe { *(in_data as *const u32) };
            let previous = wrapper
                .maximum_frames_per_slice
                .swap(maximum_frames, Ordering::Relaxed);
            if previous != maximum_frames {
                wrapper.notify_property_listeners(
                    K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
                    scope,
                    element,
                );
            }
            0
        }
        K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<u32>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let bypassed = unsafe { *(in_data as *const u32) } != 0;
            if let Some(param) = wrapper.bypass_param {
                let normalized = if bypassed { 1.0 } else { 0.0 };
                let bypass_hash = wrapper
                    .param_by_hash
                    .iter()
                    .find_map(|(hash, candidate)| (*candidate == param).then_some(*hash))
                    .expect("Missing bypass parameter hash");
                wrapper.apply_parameter_change(bypass_hash, normalized)
            } else {
                wrapper.bypass_enabled.store(bypassed, Ordering::Relaxed);
                0
            }
        }
        K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            if in_data.is_null()
                || in_data_size != mem::size_of::<AURenderCallbackStruct>() as u32
            {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let Some(callback) = wrapper.input_callbacks.get(element as usize) else {
                return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
            };
            let callback_value = unsafe { *(in_data as *const AURenderCallbackStruct) };
            callback.store(callback_value.inputProc.map(|_| callback_value));
            0
        }
        K_AUDIO_UNIT_PROPERTY_MAKE_CONNECTION
            if scope == K_AUDIO_UNIT_SCOPE_INPUT && wrapper.bus_channels(scope, element).is_some() =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<AudioUnitConnection>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            let Some(connection) = wrapper.input_connections.get(element as usize) else {
                return K_AUDIO_UNIT_ERR_INVALID_ELEMENT;
            };
            let connection_value = unsafe { *(in_data as *const AudioUnitConnection) };
            connection.store((!connection_value.sourceAudioUnit.is_null()).then_some(connection_value));
            0
        }
        K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<HostCallbackInfo>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            wrapper
                .host_callbacks
                .store(Some(unsafe { *(in_data as *const HostCallbackInfo) }));
            0
        }
        K_AUDIO_UNIT_PROPERTY_IN_PLACE_PROCESSING
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<u32>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            wrapper.in_place_processing.store(false, Ordering::Relaxed);
            0
        }
        K_AUDIO_UNIT_PROPERTY_MIDI_OUTPUT_CALLBACK
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL
                && element == 0
                && P::MIDI_OUTPUT != MidiConfig::None =>
        {
            if in_data.is_null()
                || in_data_size != mem::size_of::<AUMIDIOutputCallbackStruct>() as u32
            {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            wrapper
                .midi_output_callback
                .store(Some(unsafe { *(in_data as *const AUMIDIOutputCallbackStruct) }));
            0
        }
        K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<AUPreset>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            wrapper.restore_defaults();
            0
        }
        K_AUDIO_UNIT_PROPERTY_CURRENT_PRESET
            if scope == K_AUDIO_UNIT_SCOPE_GLOBAL && element == 0 =>
        {
            if in_data.is_null() || in_data_size != mem::size_of::<AUPreset>() as u32 {
                return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
            }
            wrapper.restore_defaults();
            0
        }
        _ => K_AUDIO_UNIT_ERR_PROPERTY_NOT_WRITABLE,
    }
}

unsafe extern "C" fn get_parameter<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    param_id: AudioUnitParameterID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out_value: *mut AudioUnitParameterValue,
) -> OSStatus {
    if out_value.is_null() {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }
    if scope != K_AUDIO_UNIT_SCOPE_GLOBAL || element != 0 {
        return K_AUDIO_UNIT_ERR_INVALID_SCOPE;
    }

    let wrapper = wrapper_from_raw::<P>(self_ptr);
    let Some(param_ptr) = wrapper.param_by_hash.get(&param_id).copied() else {
        return K_AUDIO_UNIT_ERR_INVALID_PARAMETER;
    };

    unsafe {
        *out_value = param_ptr.modulated_plain_value();
    }
    0
}

unsafe extern "C" fn set_parameter<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    param_id: AudioUnitParameterID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    in_value: AudioUnitParameterValue,
    in_buffer_offset_in_frames: u32,
) -> OSStatus {
    if scope != K_AUDIO_UNIT_SCOPE_GLOBAL || element != 0 {
        return K_AUDIO_UNIT_ERR_INVALID_SCOPE;
    }

    wrapper_from_raw::<P>(self_ptr).set_plain_parameter(
        param_id,
        in_value,
        in_buffer_offset_in_frames,
    )
}

unsafe extern "C" fn schedule_parameters<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    events: *const AudioUnitParameterEvent,
    num_events: u32,
) -> OSStatus {
    if events.is_null() && num_events != 0 {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }

    let wrapper = wrapper_from_raw::<P>(self_ptr);
    for event in unsafe { slice::from_raw_parts(events, num_events as usize) } {
        let status = wrapper.queue_parameter_event(event);
        if status != 0 {
            return status;
        }
    }

    0
}

unsafe extern "C" fn render<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_output_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    wrapper_from_raw::<P>(self_ptr).render_inner(
        io_action_flags,
        in_time_stamp,
        in_output_bus_number,
        in_number_frames,
        io_data,
    )
}

unsafe extern "C" fn reset<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    scope: AudioUnitScope,
    element: AudioUnitElement,
) -> OSStatus {
    wrapper_from_raw::<P>(self_ptr).reset_inner(scope, element)
}

unsafe extern "C" fn music_device_midi_event<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    in_status: u32,
    in_data1: u32,
    in_data2: u32,
    in_offset_sample_frame: u32,
) -> OSStatus {
    if P::MIDI_INPUT == MidiConfig::None {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY;
    }

    let status = (in_status & 0xFF) as u8;
    let midi_data = [
        status,
        (in_data1 & 0x7F) as u8,
        (in_data2 & 0x7F) as u8,
    ];
    let message_len = match status & 0xF0 {
        0xC0 | 0xD0 => 2,
        0xF0 => match status {
            0xF1 | 0xF3 => 2,
            0xF2 => 3,
            0xF6 | 0xF8 | 0xFA | 0xFB | 0xFC | 0xFE | 0xFF => 1,
            _ => return K_AUDIO_UNIT_ERR_INVALID_PARAMETER,
        },
        _ => 3,
    };

    wrapper_from_raw::<P>(self_ptr)
        .queue_midi_message(in_offset_sample_frame, &midi_data[..message_len])
}

unsafe extern "C" fn music_device_sysex<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    in_data: *const u8,
    in_length: u32,
) -> OSStatus {
    if P::MIDI_INPUT == MidiConfig::None {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY;
    }
    if in_data.is_null() && in_length != 0 {
        return K_AUDIO_UNIT_ERR_INVALID_PROPERTY_VALUE;
    }

    let midi_data = unsafe { slice::from_raw_parts(in_data, in_length as usize) };
    wrapper_from_raw::<P>(self_ptr).queue_midi_message(0, midi_data)
}

fn wrapper_from_raw<P: Auv2Plugin>(self_ptr: *mut c_void) -> &'static Wrapper<P> {
    unsafe { &*(self_ptr.cast::<Wrapper<P>>()) }
}

fn audio_unit_instance_wrappers() -> &'static Mutex<HashMap<usize, usize>> {
    AUDIO_UNIT_INSTANCE_WRAPPERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_audio_unit_instance<P: Auv2Plugin>(
    self_ptr: *mut c_void,
    instance: AudioComponentInstance,
) {
    if self_ptr.is_null() || instance.is_null() {
        return;
    }

    let wrapper = wrapper_from_raw::<P>(self_ptr);
    wrapper.component_instance.store(instance as usize);
    audio_unit_instance_wrappers()
        .lock()
        .insert(instance as usize, self_ptr as usize);
}

fn unregister_audio_unit_instance<P: Auv2Plugin>(self_ptr: *mut c_void) {
    if self_ptr.is_null() {
        return;
    }

    let instance = wrapper_from_raw::<P>(self_ptr).component_instance.swap(0);
    if instance != 0 {
        audio_unit_instance_wrappers().lock().remove(&instance);
    }
}

fn wrapper_from_audio_unit_instance<P: Auv2Plugin>(audio_unit: AudioUnit) -> Option<Arc<Wrapper<P>>> {
    let self_ptr = audio_unit_instance_wrappers()
        .lock()
        .get(&(audio_unit as usize))
        .copied()? as *const Wrapper<P>;

    unsafe { clone_wrapper_arc_from_raw(self_ptr) }
}

unsafe fn clone_wrapper_arc_from_raw<P: Auv2Plugin>(
    self_ptr: *const Wrapper<P>,
) -> Option<Arc<Wrapper<P>>> {
    if self_ptr.is_null() {
        return None;
    }

    unsafe {
        Arc::increment_strong_count(self_ptr);
        Some(Arc::from_raw(self_ptr))
    }
}

fn cocoa_factory_class_name<P: Auv2Plugin>() -> String {
    format!("NihAuv2Factory_{}", cocoa_class_suffix::<P>())
}

fn cocoa_view_class_name<P: Auv2Plugin>() -> String {
    format!("NihAuv2View_{}", cocoa_class_suffix::<P>())
}

fn cocoa_class_suffix<P: Auv2Plugin>() -> String {
    format!(
        "{}_{}_{}",
        fourcc_hex(P::AUV2_TYPE),
        fourcc_hex(P::AUV2_SUBTYPE),
        fourcc_hex(P::AUV2_MANUFACTURER)
    )
}

fn cocoa_factory_class<P: Auv2Plugin>() -> &'static Class {
    let class_name = cocoa_factory_class_name::<P>();
    if let Some(class) = Class::get(&class_name) {
        return class;
    }

    let _guard = COCOA_CLASS_REGISTRATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock();
    if let Some(class) = Class::get(&class_name) {
        return class;
    }

    let mut decl =
        ClassDecl::new(&class_name, class!(NSObject)).expect("Failed to create AUv2 Cocoa factory class");
    if let Some(protocol) = Protocol::get("AUCocoaUIBase") {
        decl.add_protocol(protocol);
    }
    unsafe {
        decl.add_method(
            sel!(interfaceVersion),
            cocoa_interface_version as extern "C" fn(&Object, Sel) -> u32,
        );
        decl.add_method(
            sel!(uiViewForAudioUnit:withSize:),
            cocoa_ui_view_for_audio_unit::<P>
                as extern "C" fn(&Object, Sel, AudioUnit, NSSize) -> *mut Object,
        );
    }

    decl.register()
}

fn cocoa_view_class<P: Auv2Plugin>() -> &'static Class {
    let class_name = cocoa_view_class_name::<P>();
    if let Some(class) = Class::get(&class_name) {
        return class;
    }

    let _guard = COCOA_CLASS_REGISTRATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock();
    if let Some(class) = Class::get(&class_name) {
        return class;
    }

    let mut decl =
        ClassDecl::new(&class_name, class!(NSView)).expect("Failed to create AUv2 Cocoa view class");
    decl.add_ivar::<*mut c_void>(COCOA_VIEW_STATE_IVAR);
    unsafe {
        decl.add_method(
            sel!(dealloc),
            cocoa_view_dealloc::<P> as extern "C" fn(&mut Object, Sel),
        );
    }

    decl.register()
}

extern "C" fn cocoa_interface_version(_this: &Object, _cmd: Sel) -> u32 {
    0
}

extern "C" fn cocoa_ui_view_for_audio_unit<P: Auv2Plugin>(
    _this: &Object,
    _cmd: Sel,
    audio_unit: AudioUnit,
    preferred_size: NSSize,
) -> *mut Object {
    let Some(wrapper) = wrapper_from_audio_unit_instance::<P>(audio_unit) else {
        return ptr::null_mut();
    };

    let view_class = cocoa_view_class::<P>();
    let (view, gui_context, editor_handle) = {
        let editor = wrapper.editor.borrow();
        let Some(editor_mutex) = editor.as_ref() else {
            return ptr::null_mut();
        };
        let editor = editor_mutex.lock();
        let (default_width, default_height) = editor.size();

        // Some hosts pass a tiny placeholder here. Until AUv2 resize requests are implemented, do
        // not allow the host's preferred size to shrink the editor below its declared size.
        let width = if preferred_size.width > 0.0 {
            preferred_size.width.max(default_width as f64)
        } else {
            default_width as f64
        };
        let height = if preferred_size.height > 0.0 {
            preferred_size.height.max(default_height as f64)
        } else {
            default_height as f64
        };

        let view: *mut Object = unsafe { msg_send![view_class, alloc] };
        let view: *mut Object =
            unsafe { msg_send![view, initWithFrame: make_ns_rect(width, height)] };
        if view.is_null() {
            return ptr::null_mut();
        }
        wrapper.cocoa_view.store(view as usize);

        let gui_context = wrapper.clone().make_gui_context();
        let editor_handle =
            editor.spawn(ParentWindowHandle::AppKitNsView(view.cast()), gui_context.clone());
        (view, gui_context, editor_handle)
    };
    wrapper.open_editor_count.fetch_add(1, Ordering::Relaxed);
    let state = Box::new(CocoaViewState {
        view,
        wrapper,
        _gui_context: gui_context,
        _editor_handle: editor_handle,
    });

    unsafe {
        (*view).set_ivar(COCOA_VIEW_STATE_IVAR, Box::into_raw(state) as *mut c_void);
        msg_send![view, autorelease]
    }
}

extern "C" fn cocoa_view_dealloc<P: Auv2Plugin>(this: &mut Object, _cmd: Sel) {
    unsafe {
        let state_ptr = *this.get_ivar::<*mut c_void>(COCOA_VIEW_STATE_IVAR);
        if !state_ptr.is_null() {
            this.set_ivar(COCOA_VIEW_STATE_IVAR, ptr::null_mut::<c_void>());
            drop(Box::from_raw(state_ptr as *mut CocoaViewState<P>));
        }

        let () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

fn bundle_url_for_class(class: &Class) -> CFURLRef {
    unsafe {
        let bundle: *mut Object = msg_send![class!(NSBundle), bundleForClass: class];
        if bundle.is_null() {
            return ptr::null();
        }

        let bundle_url: *mut Object = msg_send![bundle, bundleURL];
        if bundle_url.is_null() {
            return ptr::null();
        }

        let retained_url: *mut Object = msg_send![bundle_url, retain];
        retained_url.cast()
    }
}

fn set_event_timing<S>(event: &mut crate::prelude::NoteEvent<S>, timing: u32) {
    match event {
        crate::prelude::NoteEvent::NoteOn { timing: current, .. }
        | crate::prelude::NoteEvent::NoteOff { timing: current, .. }
        | crate::prelude::NoteEvent::Choke { timing: current, .. }
        | crate::prelude::NoteEvent::VoiceTerminated { timing: current, .. }
        | crate::prelude::NoteEvent::PolyModulation { timing: current, .. }
        | crate::prelude::NoteEvent::MonoAutomation { timing: current, .. }
        | crate::prelude::NoteEvent::PolyPressure { timing: current, .. }
        | crate::prelude::NoteEvent::PolyVolume { timing: current, .. }
        | crate::prelude::NoteEvent::PolyPan { timing: current, .. }
        | crate::prelude::NoteEvent::PolyTuning { timing: current, .. }
        | crate::prelude::NoteEvent::PolyVibrato { timing: current, .. }
        | crate::prelude::NoteEvent::PolyExpression { timing: current, .. }
        | crate::prelude::NoteEvent::PolyBrightness { timing: current, .. }
        | crate::prelude::NoteEvent::MidiChannelPressure { timing: current, .. }
        | crate::prelude::NoteEvent::MidiPitchBend { timing: current, .. }
        | crate::prelude::NoteEvent::MidiCC { timing: current, .. }
        | crate::prelude::NoteEvent::MidiProgramChange { timing: current, .. }
        | crate::prelude::NoteEvent::MidiSysEx { timing: current, .. } => *current = timing,
    }
}

fn add_event_timing<S>(event: &mut crate::prelude::NoteEvent<S>, samples: u32) {
    match event {
        crate::prelude::NoteEvent::NoteOn { timing, .. }
        | crate::prelude::NoteEvent::NoteOff { timing, .. }
        | crate::prelude::NoteEvent::Choke { timing, .. }
        | crate::prelude::NoteEvent::VoiceTerminated { timing, .. }
        | crate::prelude::NoteEvent::PolyModulation { timing, .. }
        | crate::prelude::NoteEvent::MonoAutomation { timing, .. }
        | crate::prelude::NoteEvent::PolyPressure { timing, .. }
        | crate::prelude::NoteEvent::PolyVolume { timing, .. }
        | crate::prelude::NoteEvent::PolyPan { timing, .. }
        | crate::prelude::NoteEvent::PolyTuning { timing, .. }
        | crate::prelude::NoteEvent::PolyVibrato { timing, .. }
        | crate::prelude::NoteEvent::PolyExpression { timing, .. }
        | crate::prelude::NoteEvent::PolyBrightness { timing, .. }
        | crate::prelude::NoteEvent::MidiChannelPressure { timing, .. }
        | crate::prelude::NoteEvent::MidiPitchBend { timing, .. }
        | crate::prelude::NoteEvent::MidiCC { timing, .. }
        | crate::prelude::NoteEvent::MidiProgramChange { timing, .. }
        | crate::prelude::NoteEvent::MidiSysEx { timing, .. } => {
            *timing = timing.saturating_add(samples)
        }
    }
}

fn make_ns_rect(width: f64, height: f64) -> NSRect {
    NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width, height },
    }
}

unsafe fn resize_cocoa_view(view: *mut Object, width: f64, height: f64) -> bool {
    if view.is_null() {
        return false;
    }

    let size = NSSize { width, height };

    unsafe {
        let () = msg_send![view, setFrameSize: size];

        let window: *mut Object = msg_send![view, window];
        if !window.is_null() {
            let () = msg_send![window, setContentSize: size];
        }
    }

    true
}

fn make_stream_format(sample_rate: f64, channels: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: K_AUDIO_FORMAT_LINEAR_PCM,
        mFormatFlags: K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED
            | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
        mBytesPerPacket: mem::size_of::<f32>() as u32,
        mFramesPerPacket: 1,
        mBytesPerFrame: mem::size_of::<f32>() as u32,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn stream_format_is_supported(stream_format: &AudioStreamBasicDescription) -> bool {
    stream_format.mFormatID == K_AUDIO_FORMAT_LINEAR_PCM
        && stream_format.mFormatFlags
            == (K_AUDIO_FORMAT_FLAGS_NATIVE_FLOAT_PACKED | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED)
        && stream_format.mFramesPerPacket == 1
        && stream_format.mBytesPerPacket == mem::size_of::<f32>() as u32
        && stream_format.mBytesPerFrame == mem::size_of::<f32>() as u32
        && stream_format.mBitsPerChannel == 32
        && stream_format.mChannelsPerFrame > 0
}

fn parameter_info(param_ptr: ParamPtr) -> AudioUnitParameterInfo {
    let flags = unsafe { param_ptr.flags() };
    let unit_string = unsafe { param_ptr.unit() }.trim();
    let param_name = unsafe { param_ptr.name() };
    let cf_name = CFString::new(param_name);
    let mut info = AudioUnitParameterInfo {
        name: [0; 52],
        unitName: ptr::null_mut(),
        clumpID: 0,
        cfNameString: cf_name.as_concrete_TypeRef(),
        unit: parameter_unit(param_ptr),
        minValue: unsafe { param_ptr.preview_plain(0.0) },
        maxValue: unsafe { param_ptr.preview_plain(1.0) },
        defaultValue: unsafe { param_ptr.default_plain_value() },
        flags: K_AUDIO_UNIT_PARAMETER_FLAG_GLOBAL
            | K_AUDIO_UNIT_PARAMETER_FLAG_IS_READABLE
            | K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE
            | K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING
            | K_AUDIO_UNIT_PARAMETER_FLAG_CF_NAME_RELEASE,
    };
    strlcpy(&mut info.name, param_name);
    mem::forget(cf_name);

    if unsafe { param_ptr.step_count() }.is_some() {
        info.flags |= K_AUDIO_UNIT_PARAMETER_FLAG_VALUES_HAVE_STRINGS;
    } else if !flags.contains(ParamFlags::NON_AUTOMATABLE) {
        info.flags |= K_AUDIO_UNIT_PARAMETER_FLAG_CAN_RAMP;
    }

    if !unit_string.is_empty() && info.unit == K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC {
        let cf_unit = CFString::new(unit_string);
        info.unitName = cf_unit.as_concrete_TypeRef();
        mem::forget(cf_unit);
    }

    info
}

fn parameter_unit(param_ptr: ParamPtr) -> AudioUnitParameterUnit {
    match param_ptr {
        ParamPtr::BoolParam(_) => K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN,
        ParamPtr::EnumParam(_) => K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED,
        ParamPtr::IntParam(_) => K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED,
        ParamPtr::FloatParam(_) => match unsafe { param_ptr.unit() }.trim().to_ascii_lowercase().as_str() {
            "%" => K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT,
            "s" | "sec" | "seconds" => K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS,
            "ms" => K_AUDIO_UNIT_PARAMETER_UNIT_MILLISECONDS,
            "hz" => K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ,
            "db" => K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS,
            "gain" => K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN,
            "bpm" => K_AUDIO_UNIT_PARAMETER_UNIT_BPM,
            _ => K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC,
        },
    }
}

fn class_info_dictionary<P: Auv2Plugin>(serialized_state: &[u8]) -> CFDictionary<CFString, CFType> {
    let version_key = CFString::from_static_string(CLASS_INFO_VERSION_KEY);
    let type_key = CFString::from_static_string(CLASS_INFO_TYPE_KEY);
    let subtype_key = CFString::from_static_string(CLASS_INFO_SUBTYPE_KEY);
    let manufacturer_key = CFString::from_static_string(CLASS_INFO_MANUFACTURER_KEY);
    let name_key = CFString::from_static_string(CLASS_INFO_NAME_KEY);
    let preset_number_key = CFString::from_static_string(CLASS_INFO_PRESET_NUMBER_KEY);
    let data_key = CFString::from_static_string(CLASS_INFO_DATA_KEY);

    let version = CFNumber::from(parse_auv2_version(P::VERSION) as i32);
    let type_code = CFNumber::from(fourcc(P::AUV2_TYPE) as i32);
    let subtype_code = CFNumber::from(fourcc(P::AUV2_SUBTYPE) as i32);
    let manufacturer_code = CFNumber::from(fourcc(P::AUV2_MANUFACTURER) as i32);
    let name = CFString::new(P::NAME);
    let preset_number = CFNumber::from(0);
    let data = CFData::from_buffer(serialized_state);

    CFDictionary::from_CFType_pairs(&[
        (version_key, version.as_CFType()),
        (type_key, type_code.as_CFType()),
        (subtype_key, subtype_code.as_CFType()),
        (manufacturer_key, manufacturer_code.as_CFType()),
        (name_key, name.as_CFType()),
        (preset_number_key, preset_number.as_CFType()),
        (data_key, data.as_CFType()),
    ])
}

fn parse_auv2_version(version: &str) -> u32 {
    let version = version.split_once('-').map(|(version, _)| version).unwrap_or(version);
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);

    (pack_bcd_version(major, 4) << 16)
        | (pack_bcd_version(minor, 2) << 8)
        | pack_bcd_version(patch, 2)
}

fn pack_bcd_version(mut value: u32, digits: usize) -> u32 {
    let mut result = 0;
    for shift in 0..digits {
        result |= (value % 10) << (shift * 4);
        value /= 10;
    }
    result
}

fn deserialize_class_info(dictionary: &CFDictionary<CFString, CFType>) -> Option<PluginState> {
    let data_key = CFString::from_static_string(CLASS_INFO_DATA_KEY);
    let data = dictionary.find(&data_key)?.downcast::<CFData>()?;
    unsafe { state::deserialize_json(data.bytes()) }
}

const fn fourcc(bytes: [u8; 4]) -> u32 {
    u32::from_be_bytes(bytes)
}

fn fourcc_hex(bytes: [u8; 4]) -> String {
    format!("{:08X}", fourcc(bytes))
}

/// Export one or more AUv2 plugins from this library using the provided plugin types.
#[macro_export]
macro_rules! nih_export_auv2 {
    ($($plugin_ty:ty),+ $(,)?) => {
        #[doc(hidden)]
        mod auv2 {
            use ::std::sync::{Arc, OnceLock};

            use $crate::wrapper::setup_logger;
            use $crate::wrapper::auv2::{
                AudioComponentDescription, AudioComponentPlugInInterface, Auv2BundlerMetadata,
                Auv2BundlerMetadataList, Wrapper, NIH_AUV2_FACTORY_SYMBOL, NIH_AUV2_METADATA_SYMBOL,
            };

            use super::*;

            const PLUGIN_COUNT: usize = [$(stringify!($plugin_ty)),+].len();
            static BUNDLER_METADATA: OnceLock<[Auv2BundlerMetadata; PLUGIN_COUNT]> = OnceLock::new();

            fn bundler_metadata() -> &'static [Auv2BundlerMetadata; PLUGIN_COUNT] {
                BUNDLER_METADATA.get_or_init(|| [$(Auv2BundlerMetadata::for_plugin::<$plugin_ty>()),+])
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn NihAudioUnitBundlerMetadata() -> Auv2BundlerMetadataList {
                let metadata = bundler_metadata();
                Auv2BundlerMetadataList {
                    ptr: metadata.as_ptr(),
                    len: metadata.len(),
                }
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn NihAudioUnitFactory(
                in_desc: *const AudioComponentDescription,
            ) -> *mut AudioComponentPlugInInterface {
                setup_logger();

                let Some(desc) = (unsafe { in_desc.as_ref() }) else {
                    return ::std::ptr::null_mut();
                };

                let metadata = bundler_metadata();
                let mut metadata_idx = 0;
                $({
                    if metadata[metadata_idx].matches(desc) {
                        return match Wrapper::<$plugin_ty>::new() {
                            Some(wrapper) => Arc::into_raw(wrapper) as *mut AudioComponentPlugInInterface,
                            None => ::std::ptr::null_mut(),
                        };
                    }

                    metadata_idx += 1;
                })+

                ::std::ptr::null_mut()
            }

            #[allow(dead_code)]
            const _FACTORY_SYMBOL: &str = NIH_AUV2_FACTORY_SYMBOL;
            #[allow(dead_code)]
            const _METADATA_SYMBOL: &str = NIH_AUV2_METADATA_SYMBOL;
        }
    };
}
