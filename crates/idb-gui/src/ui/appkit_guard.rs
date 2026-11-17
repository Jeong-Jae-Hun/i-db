//! AppKit Touch Bar 종료 버그 우회.
//!
//! 창을 닫거나 몇 시간 띄워 둔 뒤 AppKit 이 `-[_NSTouchBarFinderObservation invalidate]` 에서 등록하지 않은 KVO
//! 관찰자("nextResponder")를 빼려다 NSRangeException 을 던지고, 디스플레이 주기(NSDisplayCycleFlush) 관찰자 안이라
//! `+[NSApplication _crashOnException:]` 로 바로 죽는다. 앱 프레임이 하나도 없는 GPUI·AppKit 쪽 버그이고
//! Touch Bar 가 없는 Mac 도 TouchBarServer 가 돌면(Sidecar·유니버설 컨트롤 + Touch Bar 보기) 겪는다. 아직 고친 판이 없다.
//! - <https://github.com/zed-industries/zed/issues/64819>
//! - <https://github.com/longbridge/gpui-kit/issues/3192>
//!
//! 그래서 둘을 한다.
//! 1. 그 메서드 하나만 감싼다. 클래스가 그 셀렉터를 직접 갖고 꼴이 `v16@0:8`(인자 없음·void)일 때만 바꾸고,
//!    원래 구현을 `@try` 안에서 부른 뒤 본 적 있는 NSRangeException 만 삼키고 crash.log 에 "appkit exception" 으로
//!    남긴다. 다른 예외는 다시 던지고, nil 예외는 AppKit 처럼 죽는다. 클래스가 없는 macOS 에서는 아무것도 하지 않는다.
//! 2. Touch Bar 를 덜 건드리게 앱의 "Touch Bar 사용자화" 메뉴 자동 추가를 끄고 창·뷰의 touchBar 를 nil 로 둔다.
//!
//! `NSApplicationCrashOnExceptions = NO` 는 쓰지 않는다. 디스플레이 주기 관찰자는 이 값과 상관없이 죽고, 앱 전체의
//! 예외 정책을 바꿔 진짜 오류를 숨길 수 있어서다. 업스트림이 고치면 이 파일을 지운다.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_foundation::NSString;

const TOUCH_BAR_OBSERVATION: &std::ffi::CStr = c"_NSTouchBarFinderObservation";
/// 이 길에서 본 예외만 삼킨다(KVO 관찰자 빼기의 NSRangeException)
const SWALLOWED: &str = "NSRangeException";
/// `(self, _cmd) -> void` 의 64비트 꼴
const VOID_NO_ARGS: &std::ffi::CStr = c"v16@0:8";

type Original = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);

/// 감싼 (클래스, 셀렉터) 의 원래 구현이다.
static ORIGINALS: Mutex<Option<HashMap<(usize, Sel), Original>>> = Mutex::new(None);
/// 삼킨 예외 수(시험이 본다)
static SWALLOWED_COUNT: AtomicUsize = AtomicUsize::new(0);
/// crash.log 자리. 시험은 두지 않아 사용자 파일을 건드리지 않는다
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 앱을 띄우기 전에 부른다. 기록 자리를 정하고 Touch Bar 관찰 해제를 감싼다.
pub fn install(log: Option<PathBuf>) {
    if let Some(p) = log {
        let _ = LOG_PATH.set(p);
    }
    if let Some(cls) = AnyClass::get(TOUCH_BAR_OBSERVATION) {
        // SAFETY: AppKit 의 이 클래스 invalidate 는 다른 곳에서 바꾸지 않고, 꼴은 guard_method 가 확인한다.
        unsafe { guard_method(cls, sel!(invalidate)) };
    }
}

/// 2. 앱이 뜬 뒤(메인 스레드) 부른다. Touch Bar 사용자화 메뉴 자동 추가를 끈다.
pub fn quiet_app_touch_bar() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setAutomaticCustomizeTouchBarMenuItemEnabled(false);
}

/// 2. 창의 NSView·NSWindow 의 touchBar 를 nil 로 둔다.
pub fn quiet_window_touch_bar(window: &impl raw_window_handle::HasWindowHandle) {
    let Some(_mtm) = MainThreadMarker::new() else { return };
    let Ok(h) = window.window_handle() else { return };
    let raw_window_handle::RawWindowHandle::AppKit(h) = h.as_raw() else {
        return;
    };
    let view = h.ns_view.as_ptr() as *mut objc2_app_kit::NSView;
    // SAFETY: 창 핸들이 살아 있는 동안 받은 NSView 이고 메인 스레드다.
    unsafe {
        let view = &*view;
        view.setTouchBar(None);
        if let Some(w) = view.window() {
            w.setTouchBar(None);
        }
    }
}

/// cls 가 sel 을 직접 갖고(상위 클래스 것은 건드리지 않는다) 그 꼴이 `v16@0:8` 일 때만 예외를 거르는 구현으로
/// 바꾼다. 바꿨으면(이미 감쌌으면) true.
///
/// # Safety
/// 부르는 쪽은 다른 코드가 같은 메서드 구현을 동시에 바꾸지 않음을 보장한다. 바꾼 구현은 프로세스 끝까지 남는다.
/// 꼴이 다르면 바꾸지 않으므로 인자·반환 꼴이 어긋난 호출은 생기지 않는다.
pub unsafe fn guard_method(cls: &AnyClass, sel: Sel) -> bool {
    let mut originals = ORIGINALS.lock().unwrap_or_else(|e| e.into_inner());
    let map = originals.get_or_insert_with(HashMap::new);
    let key = (cls as *const AnyClass as usize, sel);
    if map.contains_key(&key) {
        return true;
    }
    let methods = cls.instance_methods();
    let Some(m) = methods.iter().find(|m| m.name() == sel) else {
        return false;
    };
    // SAFETY: 살아 있는 Method 의 꼴 글이다(런타임이 가진 정적 글).
    let enc = unsafe { objc2::ffi::method_getTypeEncoding(*m as *const _) };
    if enc.is_null() || unsafe { std::ffi::CStr::from_ptr(enc) } != VOID_NO_ARGS {
        return false;
    }
    let guarded: Original = guarded_call;
    // SAFETY: 둘 다 (self, _cmd) 만 받고 void 를 돌려주는 메서드 꼴이다. 원래 구현은 같은 꼴로 되돌려 부른다.
    unsafe {
        let old = m.set_implementation(std::mem::transmute::<Original, Imp>(guarded));
        map.insert(key, std::mem::transmute::<Imp, Original>(old));
    }
    true
}

/// 감싼 메서드가 이 구현인지다(시험용).
#[cfg(test)]
fn is_guarded(cls: &AnyClass, sel: Sel) -> bool {
    let guarded: Original = guarded_call;
    cls.instance_method(sel)
        .is_some_and(|m| m.implementation() as usize == guarded as usize)
}

#[cfg(test)]
fn swallowed_count() -> usize {
    SWALLOWED_COUNT.load(Ordering::SeqCst)
}

fn original_for(this: *mut AnyObject, cmd: Sel) -> Option<Original> {
    let originals = ORIGINALS.lock().unwrap_or_else(|e| e.into_inner());
    let map = originals.as_ref()?;
    // SAFETY: AppKit 이 넘긴 살아 있는 객체다.
    let mut cls = Some(unsafe { (*this).class() });
    while let Some(c) = cls {
        if let Some(f) = map.get(&(c as *const AnyClass as usize, cmd)) {
            return Some(*f);
        }
        cls = c.superclass();
    }
    None
}

unsafe extern "C-unwind" fn guarded_call(this: *mut AnyObject, cmd: Sel) {
    let Some(orig) = original_for(this, cmd) else { return };
    // SAFETY: 원래 구현을 받은 그대로의 인자로 부른다.
    let r = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe { orig(this, cmd) }));
    let Err(exc) = r else { return };
    let Some(exc) = exc else {
        // 무엇인지 모르는 예외(nil·메모리 부족)는 삼키지 않는다. AppKit 이 하던 대로 죽는다
        note("nil, 끝냄", cmd);
        std::process::abort();
    };
    // SAFETY: NSException 의 name 은 NSString 이다.
    let name: Option<Retained<NSString>> = unsafe { msg_send![&**exc, name] };
    let name = name.map(|n| n.to_string()).unwrap_or_default();
    if name == SWALLOWED {
        note(&name, cmd);
        return;
    }
    objc2::exception::throw(exc)
}

/// 삼킨(또는 죽기 전) 예외를 남긴다. 예외 글(reason)은 싣지 않는다: 객체 주소·키 경로 말고는 쓸모가 없고 crash.log 원칙과 같다.
fn note(name: &str, cmd: Sel) {
    SWALLOWED_COUNT.fetch_add(1, Ordering::SeqCst);
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z").to_string();
    let text = format!(
        "=== i-db {} appkit exception {when} ===\nkind: appkit exception ({name})\nlocation: {}\n",
        env!("CARGO_PKG_VERSION"),
        cmd.name().to_string_lossy()
    );
    eprintln!("{text}");
    if let Some(p) = LOG_PATH.get() {
        let _ = crate::vm::crashlog::append(p, &text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::runtime::{ClassBuilder, NSObject};
    use objc2::{ClassType as _, class};
    use objc2_foundation::NSException;

    unsafe extern "C-unwind" fn with_arg(_this: *mut AnyObject, _cmd: Sel, _x: *mut AnyObject) {}

    unsafe extern "C-unwind" fn returns_bool(_this: *mut AnyObject, _cmd: Sel) -> objc2::runtime::Bool {
        objc2::runtime::Bool::NO
    }

    /// 등록하지 않은 관찰자를 뺀다(AppKit 이 Touch Bar 에서 하는 것과 같은 NSRangeException).
    unsafe extern "C-unwind" fn misremove(_this: *mut AnyObject, _cmd: Sel) {
        let target = NSObject::new();
        let observer = NSObject::new();
        let key = NSString::from_str("nextResponder");
        let _: () = unsafe { msg_send![&*target, removeObserver: &*observer, forKeyPath: &*key] };
    }

    unsafe extern "C-unwind" fn other_error(_this: *mut AnyObject, _cmd: Sel) {
        // 본 적 없는 이름은 다시 던져야 한다(NSInternalInconsistencyException 도 삼키지 않는다)
        let e = NSException::new(
            &NSString::from_str("NSInternalInconsistencyException"),
            Some(&NSString::from_str("x")),
            None,
        )
        .expect("exception");
        // SAFETY: NSException 은 Exception 이다.
        objc2::exception::throw(unsafe { Retained::cast_unchecked(e) })
    }

    fn probe() -> &'static AnyClass {
        static CLS: OnceLock<usize> = OnceLock::new();
        let p = *CLS.get_or_init(|| {
            let mut b = ClassBuilder::new(c"IdbAppkitGuardProbe", NSObject::class()).expect("class");
            // SAFETY: (self, _cmd) -> void 꼴이 셀렉터와 맞는다.
            unsafe {
                b.add_method(sel!(withArg:), with_arg as unsafe extern "C-unwind" fn(_, _, _));
                b.add_method(sel!(returnsBool), returns_bool as unsafe extern "C-unwind" fn(_, _) -> _);
            }
            let cls = b.register();
            // AppKit 의 invalidate 와 같은 꼴 글(v16@0:8)로 단다. ClassBuilder 는 오프셋 없는 꼴을 적는다
            let void_fns: [(Sel, Original); 2] = [(sel!(misremove), misremove), (sel!(otherError), other_error)];
            for (sel, f) in void_fns {
                // SAFETY: (self, _cmd) -> void 함수를 그 꼴 글로 단다.
                let ok = unsafe {
                    objc2::ffi::class_addMethod(
                        cls as *const AnyClass as *mut AnyClass,
                        sel,
                        std::mem::transmute::<Original, Imp>(f),
                        VOID_NO_ARGS.as_ptr(),
                    )
                };
                assert!(ok.as_bool());
            }
            cls as *const AnyClass as usize
        });
        // SAFETY: 등록한 클래스는 프로세스 끝까지 산다.
        unsafe { &*(p as *const AnyClass) }
    }

    #[test]
    fn 감싼_메서드는_KVO_관찰자_빼기_예외를_삼키고_기록한다() {
        let cls = probe();
        // 감싸기 전에는 정말 던진다
        let obj: Retained<AnyObject> = unsafe { msg_send![cls, new] };
        let before = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            let _: () = msg_send![&*obj, misremove];
        }));
        assert!(before.is_err(), "시험 대상이 예외를 던지지 않는다");
        assert!(unsafe { guard_method(cls, sel!(misremove)) });
        assert!(unsafe { guard_method(cls, sel!(misremove)) }, "두 번 걸어도 한 번만 감싼다");
        assert!(is_guarded(cls, sel!(misremove)));
        let n = swallowed_count();
        let after = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            let _: () = msg_send![&*obj, misremove];
        }));
        assert!(after.is_ok(), "감싼 뒤에는 죽지 않는다");
        assert_eq!(swallowed_count(), n + 1);
    }

    #[test]
    fn 정해_둔_예외가_아니면_다시_던진다() {
        let cls = probe();
        assert!(unsafe { guard_method(cls, sel!(otherError)) });
        let obj: Retained<AnyObject> = unsafe { msg_send![cls, new] };
        let r = objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
            let _: () = msg_send![&*obj, otherError];
        }));
        assert!(r.is_err(), "다른 예외까지 삼키면 안 된다");
    }

    #[test]
    fn 상위_클래스_메서드나_없는_셀렉터는_건드리지_않는다() {
        // NSObject 의 description 은 probe 가 직접 갖지 않는다
        assert!(!unsafe { guard_method(probe(), sel!(description)) });
        assert!(!is_guarded(class!(NSObject), sel!(description)));
    }

    #[test]
    fn 꼴이_v16_0_8_이_아니면_바꾸지_않는다() {
        let cls = probe();
        assert!(!unsafe { guard_method(cls, sel!(withArg:)) }, "인자가 있는 메서드");
        assert!(!unsafe { guard_method(cls, sel!(returnsBool)) }, "void 가 아닌 메서드");
        assert!(!is_guarded(cls, sel!(withArg:)));
        assert!(!is_guarded(cls, sel!(returnsBool)));
    }

    #[test]
    fn 설치는_창_없이_돌고_Touch_Bar_관찰_클래스가_있으면_감싼다() {
        install(None);
        if let Some(cls) = AnyClass::get(TOUCH_BAR_OBSERVATION) {
            assert!(is_guarded(cls, sel!(invalidate)));
        }
    }
}
