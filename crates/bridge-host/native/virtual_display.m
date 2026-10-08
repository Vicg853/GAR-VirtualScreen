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
        CGVirtualDisplaySettings *settings = [settingsClass new];
        settings.hiDPI = 0;
        settings.modes = @[[[modeClass alloc] initWithWidth:width height:height refreshRate:fps]];
        if (![display applySettings:settings]) return NULL;
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
        CGVirtualDisplaySettings *settings=[NSClassFromString(@"CGVirtualDisplaySettings") new];
        settings.hiDPI=0;
        settings.modes=@[[[NSClassFromString(@"CGVirtualDisplayMode") alloc] initWithWidth:width height:height refreshRate:fps]];
        return [display applySettings:settings];
    } @catch(NSException *e) { return false; }
}

void airbridge_display_release(void *display) {
    if (display) CFBridgingRelease(display);
}

typedef void (*AirBridgeVideoFrameCallback)(void *context, const uint8_t *bytes, size_t length, size_t row_bytes);

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
                                   uint32_t fps, void *context,
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
        (__bridge NSString *)frame_time_key: @(1.0 / fps),
        (__bridge NSString *)depth_key: @3,
    };
    capture.stream = create_stream(
        display_id, width, height, kCVPixelFormatType_32BGRA,
        (__bridge CFDictionaryRef)properties, queue,
        ^(CGDisplayStreamFrameStatus status, uint64_t display_time,
          IOSurfaceRef surface, CGDisplayStreamUpdateRef update) {
            (void)display_time;
            (void)update;
            if (status == kCGDisplayStreamFrameStatusStopped) {
                dispatch_semaphore_signal(stopped);
                return;
            }
            if (status != kCGDisplayStreamFrameStatusFrameComplete || !surface) return;
            if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess) return;
            const uint8_t *base = IOSurfaceGetBaseAddress(surface);
            size_t row_bytes = IOSurfaceGetBytesPerRow(surface);
            size_t height = IOSurfaceGetHeight(surface);
            if (base && row_bytes && height && height <= SIZE_MAX / row_bytes) {
                callback(context, base, row_bytes * height, row_bytes);
            }
            IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
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
