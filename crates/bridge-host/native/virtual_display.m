#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <dispatch/dispatch.h>

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
        descriptor.maxPixelsWide = width;
        descriptor.maxPixelsHigh = height;
        descriptor.sizeInMillimeters = CGSizeMake(width * 0.2646, height * 0.2646);
        descriptor.vendorID = 0xABCD;
        descriptor.productID = 0xA1B1;
        descriptor.serialNum = 1;
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

void airbridge_display_release(void *display) {
    if (display) CFBridgingRelease(display);
}

