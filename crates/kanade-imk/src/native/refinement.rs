//! Read-only, bounded candidate refreshes while a local ranker is pending.
//! The timer holds a weak controller and never fetches or edits document text.
use super::*;
use objc2::rc::Weak;
use objc2_foundation::NSTimer;

#[derive(Default)]
pub(super) struct Polling {
    timer: RefCell<Option<Retained<NSTimer>>>,
    attempts: Cell<u8>,
}
impl Drop for Polling {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.get_mut().take() {
            timer.invalidate();
        }
    }
}
struct TargetState {
    controller: Weak<Controller>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetState]
    #[name = "KanadeCandidatePollTarget"]
    struct PollTarget;
    impl PollTarget {
        #[unsafe(method(pollCandidates:))]
        fn tick(&self, timer: &NSTimer) {
            // Invalidation may release the timer's reference to this target.
            let _keep_alive = self.retain();
            guarded(|| {
                if let Some(controller) = self.ivars().controller.load() {
                    controller.poll_refinement();
                } else {
                    timer.invalidate();
                }
            });
        }
    }
);
impl Controller {
    pub(super) fn stop_refinement(&self) {
        let timer = self
            .ivars()
            .refinement
            .timer
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(timer) = timer {
            timer.invalidate();
        }
    }
    pub(super) fn schedule_refinement(&self, pending: bool) {
        self.stop_refinement();
        if !pending {
            return;
        }
        self.ivars().refinement.attempts.set(0);
        let target = PollTarget::alloc(self.mtm()).set_ivars(TargetState {
            controller: Weak::new(self),
        });
        // SAFETY: NSObject's designated initializer; the target and timer are
        // created on the controller's main thread and use that run loop only.
        let target: Retained<PollTarget> = unsafe { msg_send![super(target), init] };
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.06,
                &target,
                sel!(pollCandidates:),
                None,
                true,
            )
        };
        if let Ok(mut slot) = self.ivars().refinement.timer.try_borrow_mut() {
            *slot = Some(timer);
        } else {
            timer.invalidate();
        }
    }
    fn poll_refinement(&self) {
        if unsafe { IsSecureEventInputEnabled() } {
            self.clear_context();
            return;
        }
        let attempts = self.ivars().refinement.attempts.get();
        let expired = attempts >= 12;
        self.ivars()
            .refinement
            .attempts
            .set(attempts.saturating_add(1));
        let page = self.ivars().page.try_borrow().ok().and_then(|p| p.clone());
        let sender = self
            .ivars()
            .sender
            .try_borrow()
            .ok()
            .and_then(|s| s.clone());
        let (Some(mut page), Some(sender)) = (page, sender) else {
            self.stop_refinement();
            return;
        };
        if !page.refinement_pending {
            self.stop_refinement();
            return;
        }
        let app: Option<Retained<NSString>> = unsafe { msg_send![&*sender, bundleIdentifier] };
        let front = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .and_then(|a| a.bundleIdentifier());
        if app.is_none() || app.as_deref() != front.as_deref() {
            self.hide();
            return;
        }
        let client = self
            .ivars()
            .engine
            .try_borrow_mut()
            .ok()
            .and_then(|mut c| c.take());
        let Some(mut client) = client else {
            if expired {
                page.mark_refinement_timeout();
                self.show(&page, &sender);
            }
            return;
        };
        let result = client.poll_candidates(&page);
        if let Ok(mut stored) = self.ivars().engine.try_borrow_mut() {
            *stored = Some(client);
        }
        match result {
            Ok(Some(update)) => self.show(&update, &sender),
            Ok(None) if expired => {
                page.mark_refinement_timeout();
                self.show(&page, &sender);
            }
            Ok(None) => {}
            Err(_) => self.hide(),
        }
    }
}
