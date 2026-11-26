#import <Foundation/Foundation.h>
#import <WebKit/WebKit.h>
#import <objc/runtime.h>

#include <unordered_map>

#include "bindings/bindings.h"

extern "C" void legion_install_ios_immersive_bridge(void);
extern "C" void legion_ios_attach_webview(void *webview_ptr);
extern "C" void legion_ios_reset_player_state(void);
extern "C" void legion_install_ios_audio_bridge(void);
extern "C" void legion_ios_audio_attach_webview(void *webview_ptr);
extern "C" void legion_ios_audio_reset(void);

@interface WKWebView (LegionImmersiveHook)
- (instancetype)legion_initWithFrame:(CGRect)frame
                       configuration:(WKWebViewConfiguration *)configuration
    __attribute__((objc_method_family(init)));
- (void)legion_setNavigationDelegate:(id<WKNavigationDelegate>)navigationDelegate;
@end

namespace {

std::unordered_map<const void *, IMP> &terminationOriginals() {
  static std::unordered_map<const void *, IMP> originals;
  return originals;
}

void legion_webViewWebContentProcessDidTerminate(id self, SEL _cmd, WKWebView *webView) {
  legion_ios_reset_player_state();
  legion_ios_audio_reset();
  Class cls = object_getClass(self);
  auto &originals = terminationOriginals();
  while (cls != Nil) {
    auto it = originals.find((__bridge const void *)cls);
    if (it != originals.end()) {
      if (it->second != nullptr) {
        reinterpret_cast<void (*)(id, SEL, WKWebView *)>(it->second)(self, _cmd, webView);
      }
      return;
    }
    cls = class_getSuperclass(cls);
  }
}

void hookNavigationDelegateTermination(id<WKNavigationDelegate> delegate) {
  if (delegate == nil) {
    return;
  }
  Class cls = object_getClass(delegate);
  if (cls == Nil) {
    return;
  }
  const void *key = (__bridge const void *)cls;
  auto &originals = terminationOriginals();
  if (originals.find(key) != originals.end()) {
    return;
  }

  SEL sel = @selector(webViewWebContentProcessDidTerminate:);
  Method method = class_getInstanceMethod(cls, sel);
  IMP replacement = reinterpret_cast<IMP>(legion_webViewWebContentProcessDidTerminate);
  if (method == nullptr) {
    originals.emplace(key, nullptr);
    class_addMethod(cls, sel, replacement, "v@:@");
    return;
  }
  IMP original = method_getImplementation(method);
  if (original == replacement) {
    return;
  }
  originals.emplace(key, original);
  if (!class_addMethod(cls, sel, replacement, method_getTypeEncoding(method))) {
    method_setImplementation(method, replacement);
  }
}

void installWebViewCreationHooks() {
  static dispatch_once_t onceToken;
  dispatch_once(&onceToken, ^{
    Class cls = [WKWebView class];
    Method origInit = class_getInstanceMethod(cls, @selector(initWithFrame:configuration:));
    Method swizzledInit =
        class_getInstanceMethod(cls, @selector(legion_initWithFrame:configuration:));
    if (origInit != nullptr && swizzledInit != nullptr) {
      method_exchangeImplementations(origInit, swizzledInit);
    }

    Method origSetNav = class_getInstanceMethod(cls, @selector(setNavigationDelegate:));
    Method swizzledSetNav = class_getInstanceMethod(cls, @selector(legion_setNavigationDelegate:));
    if (origSetNav != nullptr && swizzledSetNav != nullptr) {
      method_exchangeImplementations(origSetNav, swizzledSetNav);
    }
  });
}

}  // namespace

@implementation WKWebView (LegionImmersiveHook)

- (instancetype)legion_initWithFrame:(CGRect)frame
                       configuration:(WKWebViewConfiguration *)configuration {
  if (configuration != nil) {
    configuration.allowsInlineMediaPlayback = YES;
    configuration.allowsPictureInPictureMediaPlayback = YES;
  }
  WKWebView *webView = [self legion_initWithFrame:frame configuration:configuration];
  if (webView != nil) {
    legion_ios_attach_webview((__bridge void *)webView);
    legion_ios_audio_attach_webview((__bridge void *)webView);
  }
  return webView;
}

- (void)legion_setNavigationDelegate:(id<WKNavigationDelegate>)navigationDelegate {
  hookNavigationDelegateTermination(navigationDelegate);
  [self legion_setNavigationDelegate:navigationDelegate];
}

@end

int main(int argc, char *argv[]) {
  installWebViewCreationHooks();
  legion_install_ios_immersive_bridge();
  legion_install_ios_audio_bridge();
  ffi::start_app();
  return 0;
}
