#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>
#import <dispatch/dispatch.h>
#import <dlfcn.h>

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(NSUInteger)width height:(NSUInteger)height refreshRate:(CGFloat)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(retain, nonatomic) NSArray<CGVirtualDisplayMode *> *modes;
@property(nonatomic) unsigned int hiDPI;
@end

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) dispatch_queue_t queue;
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int serialNum;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@end

@interface CGVirtualDisplay : NSObject
@property(readonly, nonatomic) CGDirectDisplayID displayID;
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@end

// The mode macOS currently drives a display with, in pixels. False while the
// display is not (yet) online.
bool airbridge_display_mode(uint32_t display_id, uint32_t *width, uint32_t *height, uint32_t *fps) {
    if (!CGDisplayIsOnline(display_id)) return false;
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display_id);
    if (!mode) return false;
    *width = (uint32_t)CGDisplayModeGetPixelWidth(mode);
    *height = (uint32_t)CGDisplayModeGetPixelHeight(mode);
    *fps = (uint32_t)(CGDisplayModeGetRefreshRate(mode) + 0.5);
    CGDisplayModeRelease(mode);
    return *width > 0 && *height > 0;
}

static bool airbridge_mode_is(uint32_t display_id, uint32_t width, uint32_t height) {
    uint32_t w = 0, h = 0, fps = 0;
    return airbridge_display_mode(display_id, &w, &h, &fps) && w == width && h == height;
}

// The requested mode first, then the common resolutions in the same
// orientation, so the screen can also be switched from System Settings.
static NSArray *airbridge_modes(uint32_t width, uint32_t height, uint32_t fps) {
    static const uint32_t common[][2] = {
        {1280, 720}, {1920, 1080}, {1920, 1200}, {2560, 1440},
        {2560, 1600}, {3024, 1964}, {3440, 1440}, {3840, 2160},
    };
    Class modeClass = NSClassFromString(@"CGVirtualDisplayMode");
    NSMutableArray *modes = [NSMutableArray array];
    [modes addObject:[[modeClass alloc] initWithWidth:width height:height refreshRate:fps]];
    BOOL portrait = height > width;
    for (size_t i = 0; i < sizeof(common) / sizeof(common[0]); i++) {
        uint32_t w = portrait ? common[i][1] : common[i][0];
        uint32_t h = portrait ? common[i][0] : common[i][1];
        if (w == width && h == height) continue;
        [modes addObject:[[modeClass alloc] initWithWidth:w height:h refreshRate:fps]];
    }
    return modes;
}

static void airbridge_select_mode(uint32_t display_id, uint32_t width, uint32_t height) {
    CFArrayRef all = CGDisplayCopyAllDisplayModes(display_id, NULL);
    if (!all) return;
    for (CFIndex i = 0; i < CFArrayGetCount(all); i++) {
        CGDisplayModeRef mode = (CGDisplayModeRef)CFArrayGetValueAtIndex(all, i);
        if (CGDisplayModeGetPixelWidth(mode) == width && CGDisplayModeGetPixelHeight(mode) == height) {
            CGDisplaySetDisplayMode(display_id, mode, NULL);
            break;
        }
    }
    CFRelease(all);
}

static bool airbridge_wait_online(uint32_t display_id) {
    uint32_t w = 0, h = 0, fps = 0;
    for (int attempt = 0; attempt < 20; attempt++) {
        if (airbridge_display_mode(display_id, &w, &h, &fps)) return true;
        usleep(25000);
    }
    return false;
}

// Offer several modes and make the requested one current. If macOS will not
// switch to it, fall back to offering that mode alone, which always applies.
static bool airbridge_apply_mode(CGVirtualDisplay *display, uint32_t width, uint32_t height, uint32_t fps) {
    Class settingsClass = NSClassFromString(@"CGVirtualDisplaySettings");
    Class modeClass = NSClassFromString(@"CGVirtualDisplayMode");
    CGVirtualDisplaySettings *settings = [settingsClass new];
    settings.hiDPI = 0;
    settings.modes = airbridge_modes(width, height, fps);
    if ([display applySettings:settings]) {
        uint32_t display_id = display.displayID;
        if (airbridge_wait_online(display_id)) {
            if (airbridge_mode_is(display_id, width, height)) return true;
            airbridge_select_mode(display_id, width, height);
            for (int attempt = 0; attempt < 20; attempt++) {
                if (airbridge_mode_is(display_id, width, height)) return true;
                usleep(25000);
            }
        }
    }
    CGVirtualDisplaySettings *only = [settingsClass new];
    only.hiDPI = 0;
    only.modes = @[[[modeClass alloc] initWithWidth:width height:height refreshRate:fps]];
    return [display applySettings:only];
}

// The returned object must stay alive for the lifetime of the extended desktop.
void *airbridge_display_create(uint32_t width, uint32_t height, uint32_t fps, uint32_t *display_id) {
    @try {
        Class descriptorClass = NSClassFromString(@"CGVirtualDisplayDescriptor");
        Class displayClass = NSClassFromString(@"CGVirtualDisplay");
        Class modeClass = NSClassFromString(@"CGVirtualDisplayMode");
        Class settingsClass = NSClassFromString(@"CGVirtualDisplaySettings");
        if (!descriptorClass || !displayClass || !modeClass || !settingsClass) return NULL;
        CGVirtualDisplayDescriptor *descriptor = [descriptorClass new];
        descriptor.name = @"AirBridge";
        descriptor.queue = dispatch_get_main_queue();
        descriptor.maxPixelsWide = 7680;
        descriptor.maxPixelsHigh = 4320;
        descriptor.sizeInMillimeters = CGSizeMake(width * 0.2646, height * 0.2646);
        descriptor.vendorID = 0xABCD;
        descriptor.productID = 0xA1B1;
        static unsigned int nextSerial = 1;
        @synchronized(displayClass) { descriptor.serialNum = nextSerial++; }
        CGVirtualDisplay *display = [[displayClass alloc] initWithDescriptor:descriptor];
        if (!display) return NULL;
        if (!airbridge_apply_mode(display, width, height, fps)) return NULL;
        // Only ever hand out the display created here: never a built-in or
        // otherwise physical monitor that happens to share an identifier.
        if (display.displayID == 0 || CGDisplayIsBuiltin(display.displayID)) return NULL;
        *display_id = display.displayID;
        return (__bridge_retained void *)display;
    } @catch (NSException *exception) {
        NSLog(@"AirBridge virtual display error: %@", exception);
        return NULL;
    }
}


bool airbridge_display_update(void *ptr,uint32_t width,uint32_t height,uint32_t fps) {
    @try {
        CGVirtualDisplay *display = (__bridge CGVirtualDisplay *)ptr;
        if (!display) return false;
        // Already in the requested mode (for example after the user switched it
        // in System Settings): leave the display alone.
        uint32_t w = 0, h = 0, current = 0;
        if (airbridge_display_mode(display.displayID, &w, &h, &current) && w == width && h == height
            && (current == 0 || current == fps)) return true;
        return airbridge_apply_mode(display, width, height, fps);
    } @catch(NSException *e) { return false; }
}

void airbridge_display_release(void *display) {
    if (display) CFBridgingRelease(display);
}

typedef void (*AirBridgeVideoFrameCallback)(void *context, IOSurfaceRef surface);

@interface AirBridgeVideoStream : NSObject
@property(nonatomic) CGDisplayStreamRef stream;
@property(nonatomic,strong) dispatch_queue_t queue;
@property(nonatomic) BOOL started;
@property(nonatomic, strong) dispatch_semaphore_t stopped;
@end

@implementation AirBridgeVideoStream
@end

typedef CGDisplayStreamRef (*AirBridgeCreateStream)(CGDirectDisplayID, size_t, size_t,
    int32_t, CFDictionaryRef, dispatch_queue_t, CGDisplayStreamFrameAvailableHandler);
typedef CGError (*AirBridgeStreamAction)(CGDisplayStreamRef);

static AirBridgeCreateStream airbridge_create_stream(void) {
    return (AirBridgeCreateStream)dlsym(RTLD_DEFAULT, "CGDisplayStreamCreateWithDispatchQueue");
}

static AirBridgeStreamAction airbridge_stream_action(const char *name) {
    return (AirBridgeStreamAction)dlsym(RTLD_DEFAULT, name);
}

static CFStringRef airbridge_stream_key(const char *name) {
    const CFStringRef *key = dlsym(RTLD_DEFAULT, name);
    return key ? *key : NULL;
}

// CGDisplayStream captures display pixels without constructing an SCStream or
// asking CoreAudio for the clock that can stall video-only connections.
void *airbridge_video_stream_start(uint32_t display_id, uint32_t width, uint32_t height,
                                   uint32_t fps, bool vsync, void *context,
                                   AirBridgeVideoFrameCallback callback, int32_t *error_code) {
    if (error_code) *error_code = 0;
    AirBridgeCreateStream create_stream = airbridge_create_stream();
    AirBridgeStreamAction start_stream = airbridge_stream_action("CGDisplayStreamStart");
    CFStringRef cursor_key = airbridge_stream_key("kCGDisplayStreamShowCursor");
    CFStringRef frame_time_key = airbridge_stream_key("kCGDisplayStreamMinimumFrameTime");
    CFStringRef depth_key = airbridge_stream_key("kCGDisplayStreamQueueDepth");
    if (!create_stream || !start_stream || !cursor_key || !frame_time_key || !depth_key) {
        if (error_code) *error_code = -2;
        return NULL;
    }
    AirBridgeVideoStream *capture = [AirBridgeVideoStream new];
    capture.stopped = dispatch_semaphore_create(0);
    dispatch_semaphore_t stopped = capture.stopped;
    dispatch_queue_t queue = dispatch_queue_create("dev.airbridge.video-capture", DISPATCH_QUEUE_SERIAL);
    capture.queue = queue;
    NSDictionary *properties = @{
        (__bridge NSString *)cursor_key: @YES,
        // Paced to the display refresh, or delivered as soon as pixels change.
        (__bridge NSString *)frame_time_key: @(vsync ? 1.0 / fps : 0.0),
        (__bridge NSString *)depth_key: @6,
    };
    typedef const CGRect *(*UpdateRects)(CGDisplayStreamUpdateRef, CGDisplayStreamUpdateRectType, size_t *);
    UpdateRects update_rects = (UpdateRects)dlsym(RTLD_DEFAULT, "CGDisplayStreamUpdateGetRects");
    typedef size_t (*UpdateDropCount)(CGDisplayStreamUpdateRef);
    UpdateDropCount drop_count = (UpdateDropCount)dlsym(RTLD_DEFAULT, "CGDisplayStreamUpdateGetDropCount");
    __block BOOL delivered = NO;
    capture.stream = create_stream(
        display_id, width, height, kCVPixelFormatType_32BGRA,
        (__bridge CFDictionaryRef)properties, queue,
        ^(CGDisplayStreamFrameStatus status, uint64_t display_time,
          IOSurfaceRef surface, CGDisplayStreamUpdateRef update) {
            (void)display_time;
            if (status == kCGDisplayStreamFrameStatusStopped) {
                dispatch_semaphore_signal(stopped);
                return;
            }
            if (status != kCGDisplayStreamFrameStatusFrameComplete || !surface) return;
            if (delivered && update && update_rects && drop_count && !drop_count(update)) {
                // Dirty rectangles are the union of refreshed and moved regions.
                size_t dirty = 0;
                update_rects(update, kCGDisplayStreamUpdateDirtyRects, &dirty);
                if (!dirty) return;
            }
            delivered = YES;
            callback(context, surface);
        });
    if (!capture.stream) {
        if (error_code) *error_code = -1001;
        dispatch_sync(queue, ^{});
        return NULL;
    }
    CGError error = start_stream(capture.stream);
    if (error != kCGErrorSuccess) {
        if (error_code) *error_code = error;
        // Return ownership even on partial start failure. Rust must quiesce
        // callbacks before reclaiming its context; NULL would imply no producer.
        return (__bridge_retained void *)capture;
    }
    capture.started = YES;
    return (__bridge_retained void *)capture;
}

// Returns false when macOS did not confirm that all callbacks stopped. The
// caller must keep its callback context alive in that case.
bool airbridge_video_stream_stop(void *stream) {
    if (!stream) return true;
    AirBridgeVideoStream *capture = (__bridge AirBridgeVideoStream *)stream;
    AirBridgeStreamAction stop_stream = airbridge_stream_action("CGDisplayStreamStop");
    CGError error = stop_stream ? stop_stream(capture.stream) : kCGErrorFailure;
    long finished = dispatch_semaphore_wait(capture.stopped,
                                             dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
    if (error != kCGErrorSuccess || finished != 0) return false;
    dispatch_sync(capture.queue, ^{});
    CFRelease(capture.stream);
    capture.stream = NULL;
    CFBridgingRelease(stream);
    return true;
}

// Session arrangement belongs to the source host, never to a viewer window.
bool airbridge_display_position(uint32_t display_id, int32_t *x, int32_t *y) {
    if (!CGDisplayIsOnline(display_id)) return false;
    CGRect bounds = CGDisplayBounds(display_id);
    *x = (int32_t)bounds.origin.x; *y = (int32_t)bounds.origin.y;
    return true;
}
int32_t airbridge_display_set_position(uint32_t display_id, int32_t x, int32_t y) {
    CGDisplayConfigRef config = NULL;
    CGError error = CGBeginDisplayConfiguration(&config);
    if (error != kCGErrorSuccess) return error;
    error = CGConfigureDisplayOrigin(config, display_id, x, y);
    if (error != kCGErrorSuccess) { CGCancelDisplayConfiguration(config); return error; }
    return CGCompleteDisplayConfiguration(config, kCGConfigureForSession);
}
