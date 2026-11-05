use super::{
    ClickMethod, Element, Page,
    element::{own_remote_object, validate_exception},
};
use crate::{
    Error, Result, Session,
    cdp::{input::commands::InsertText, runtime::commands::CallFunctionOnResponse},
    error::{decode_ref, with_cleanup},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Modifier flags applied to both key-down and key-up events.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyModifiers {
    pub alt: bool,
    pub control: bool,
    pub meta: bool,
    pub shift: bool,
}

impl KeyModifiers {
    fn bits(self) -> i64 {
        i64::from(self.alt)
            | (i64::from(self.control) << 1)
            | (i64::from(self.meta) << 2)
            | (i64::from(self.shift) << 3)
    }
}

async fn insert_text(session: &Session, text: &str) -> Result<()> {
    session.execute(&InsertText::new(text)).await?;
    Ok(())
}

impl Page {
    /// Insert Unicode text into the focused input, without synthesizing key events.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        insert_text(&self.session, text).await
    }

    /// Press a single character or a named editing/navigation key.
    /// Named keys: Enter, Tab, Escape, Backspace, Delete, Insert, Home, End,
    /// PageUp, PageDown, ArrowLeft, ArrowRight, ArrowUp, ArrowDown, F1 through F12.
    /// For arbitrary physical-key events, use the typed Input commands directly.
    pub async fn press_key(&self, key: &str) -> Result<()> {
        self.press_key_with_modifiers(key, KeyModifiers::default())
            .await
    }

    /// Dispatch a press/release pair that continues after caller cancellation.
    ///
    /// The key is a literal logical key, not a physical keyboard-layout mapping.
    /// Shift does not uppercase character text: pass `"A"` with Shift for an
    /// uppercase A. Control, Meta, or Alt suppress the text payload while leaving
    /// the supplied key unchanged. For arbitrary text use [`Self::insert_text`];
    /// for physical-key sequences use the typed Input commands.
    ///
    /// Each command has its own deadline, capped at 30 seconds while preserving
    /// a shorter handle deadline. Release is attempted even if pressing fails.
    pub async fn press_key_with_modifiers(&self, key: &str, modifiers: KeyModifiers) -> Result<()> {
        let (virtual_key, text) = match key {
            "Enter" => (13, "\r"),
            "Tab" => (9, ""),
            "Escape" => (27, ""),
            "Backspace" => (8, ""),
            "Delete" => (46, ""),
            "Insert" => (45, ""),
            "Home" => (36, ""),
            "End" => (35, ""),
            "PageUp" => (33, ""),
            "PageDown" => (34, ""),
            "ArrowLeft" => (37, ""),
            "ArrowUp" => (38, ""),
            "ArrowRight" => (39, ""),
            "ArrowDown" => (40, ""),
            _ if key.chars().count() == 1 => {
                let character = key.chars().next().expect("one character");
                if character.is_control() {
                    return Err(Error::InvalidArgument(
                        "use a named key for control characters".into(),
                    ));
                }
                let code = if character.is_ascii_alphanumeric() || character == ' ' {
                    i64::from(character.to_ascii_uppercase() as u32)
                } else {
                    0
                };
                (code, key)
            }
            _ => {
                let number = key
                    .strip_prefix('F')
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|n| (1..=12).contains(n) && key == format!("F{n}"));
                match number {
                    Some(n) => (111 + i64::from(n), ""),
                    None => return Err(Error::InvalidArgument(format!("unsupported key: {key}"))),
                }
            }
        };
        let mut down = json!({"type":"keyDown","key":key,"modifiers":modifiers.bits()});
        if virtual_key != 0 {
            down["windowsVirtualKeyCode"] = virtual_key.into();
        }
        let code = if key.len() > 1 && key.is_ascii() {
            Some(key.to_owned())
        } else if key == " " {
            Some("Space".into())
        } else if key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() {
            Some(format!("Key{}", key.to_ascii_uppercase()))
        } else if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() {
            Some(format!("Digit{key}"))
        } else {
            None
        };
        if let Some(code) = code {
            down["code"] = code.into();
        }
        let mut up = down.clone();
        up["type"] = "keyUp".into();
        if !text.is_empty() && !modifiers.control && !modifiers.meta && !modifiers.alt {
            down["text"] = text.into();
            down["unmodifiedText"] = text.into();
        }
        dispatch_pair(
            &self.session,
            self.context_lease.clone(),
            "Input.dispatchKeyEvent",
            down,
            up,
        )
        .await
    }
}

impl Element {
    pub async fn focus(&self) -> Result<()> {
        self.0
            .session
            .execute_raw("DOM.focus", json!({"objectId":self.0.id}))
            .await?;
        Ok(())
    }

    /// Focus this element and insert Unicode text at the current selection.
    ///
    /// Uses Chrome's input pipeline without synthesizing key events. Selected text
    /// is replaced; the entire value is not cleared first. Focus and insertion are
    /// separate commands, so this operation is not atomic.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.focus().await?;
        insert_text(&self.0.session, text).await
    }

    /// Replace all text in a writable textarea or text/search/tel/url/password input.
    /// Other input types, contenteditable, disconnected, disabled, and readonly
    /// controls are rejected before editing. Native focus and selection precede
    /// Unicode insertion through Chrome's input pipeline; an empty replacement
    /// uses a Backspace press/release pair unless the control is already empty.
    /// No extra change event, blur, or submission is synthesized.
    ///
    /// Once started, the owned operation retains this element and continues after
    /// caller cancellation. Each command is capped at 30 seconds, preserving a
    /// shorter handle deadline. This is not atomic: scripts or concurrent input
    /// can change focus, selection, or value between commands. Errors and timeouts
    /// can follow side effects; the operation is never retried or rolled back.
    /// Received exception object IDs are released best-effort, not owned by errors.
    pub async fn fill(&self, text: &str) -> Result<()> {
        self.fill_until(text, None).await
    }

    pub(super) async fn fill_until(
        &self,
        text: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<()> {
        let element = self.clone();
        let text = text.to_owned();
        let limit = Duration::from_secs(30);
        let timeout = self
            .0
            .session
            .connection()
            .command_timeout()
            .unwrap_or(limit)
            .min(limit);
        let session = self.0.session.clone().with_command_timeout(Some(timeout));
        let task = self.0.session.connection().runtime().spawn(async move {
            let result = async {
                check_fill_deadline(deadline)?;
                let response = session.execute_raw_before("Runtime.callFunctionOn", json!({
                    "objectId": element.object_id(),
                    "functionDeclaration": r#"function() {
                        const view = this.ownerDocument && this.ownerDocument.defaultView;
                        const input = view && this instanceof view.HTMLInputElement;
                        const textarea = view && this instanceof view.HTMLTextAreaElement;
                        const supported = () => textarea || (input &&
                            ['text', 'search', 'tel', 'url', 'password'].includes(this.type));
                        if (!supported()) return 'fill requires a textarea or text/search/tel/url/password input';
                        const editable = () => this.isConnected && !this.matches(':disabled') && !this.readOnly;
                        if (!editable()) return 'fill requires a connected, enabled, writable control';
                        view.HTMLElement.prototype.focus.call(this);
                        if (!supported() || !editable()) return 'fill control changed while focusing';
                        // CDP input targets this session even when its document is not the active tab.
                        const focused = () => this.getRootNode().activeElement === this;
                        if (!focused()) return 'fill could not focus the control';
                        const prototype = textarea ? view.HTMLTextAreaElement.prototype : view.HTMLInputElement.prototype;
                        prototype.setSelectionRange.call(this, 0, this.value.length);
                        if (!supported() || !editable() || !focused() ||
                            this.selectionStart !== 0 || this.selectionEnd !== this.value.length)
                            return 'fill could not select all text in the control';
                        return this.value.length === 0;
                    }"#,
                    "userGesture": true,
                    "returnByValue": true,
                }), deadline).await?;
                // Share ownership with any result/exception that aliases the
                // original element, and release other received IDs exactly once.
                let mut objects = vec![element.clone()];
                for object in [&response["result"], &response["exceptionDetails"]["exception"]] {
                    own_remote_object(&element.0.session, &element.0.context_lease, object, &mut objects);
                }
                drop(objects);
                let decoded: CallFunctionOnResponse =
                    decode_ref("Runtime.callFunctionOn", &response)?;
                validate_exception("Runtime.callFunctionOn", &response)?;
                if let Some(error) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                // Preparation returns only a by-value boolean or rejection string.
                // Validate the envelope before treating a string as caller error.
                let remote = &response["result"];
                if remote.get("objectId").is_some()
                    || remote.get("unserializableValue").is_some()
                    || remote.get("subtype").is_some()
                {
                    return Err(Error::UnexpectedResponse {
                        method: "Runtime.callFunctionOn".into(),
                        payload: response,
                    });
                }
                let empty = match (decoded.result.type_.as_str(), &remote["value"]) {
                    ("boolean", Value::Bool(empty)) => *empty,
                    ("string", Value::String(message)) => {
                        return Err(Error::InvalidArgument(message.clone()));
                    }
                    _ => return Err(Error::UnexpectedResponse {
                        method: "Runtime.callFunctionOn".into(),
                        payload: response,
                    }),
                };
                check_fill_deadline(deadline)?;
                if !text.is_empty() {
                    session.execute_before(&InsertText::new(text), deadline).await.map(|_| ())
                } else if !empty {
                    dispatch_pair_before(
                        &session,
                        element.0.context_lease.clone(),
                        "Input.dispatchKeyEvent",
                        json!({"type":"keyDown","key":"Backspace","code":"Backspace","windowsVirtualKeyCode":8,"modifiers":0}),
                        json!({"type":"keyUp","key":"Backspace","code":"Backspace","windowsVirtualKeyCode":8,"modifiers":0}),
                        deadline,
                        Some(element.clone()),
                    ).await
                } else {
                    Ok(())
                }
            }.await;
            drop(element);
            result
        });
        if let Some(deadline) = deadline {
            let result = tokio::time::timeout_at(deadline, task)
                .await
                .map_err(|_| Error::Timeout("filling element".into()))??;
            check_fill_deadline(Some(deadline))?;
            result
        } else {
            task.await?
        }
    }
}

fn check_fill_deadline(deadline: Option<tokio::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        Err(Error::Timeout("filling element".into()))
    } else {
        Ok(())
    }
}

pub(super) async fn click_cdp(
    session: &Session,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
    x: f64,
    y: f64,
) -> Result<()> {
    click_cdp_owned(session, context_lease, x, y, None, None).await
}

pub(super) async fn click_cdp_before(
    element: &Element,
    x: f64,
    y: f64,
    deadline: Instant,
) -> Result<()> {
    click_cdp_owned(
        &element.0.session,
        element.0.context_lease.clone(),
        x,
        y,
        Some(deadline),
        Some(element.clone()),
    )
    .await
}

async fn click_cdp_owned(
    session: &Session,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
    x: f64,
    y: f64,
    deadline: Option<Instant>,
    element: Option<Element>,
) -> Result<()> {
    if !x.is_finite() || !y.is_finite() {
        return Err(Error::InvalidArgument(
            "click coordinates must be finite".into(),
        ));
    }
    dispatch_pair_before(
        session,
        context_lease,
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
        deadline,
        element,
    )
    .await
}

async fn dispatch_pair(
    session: &Session,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
    method: &'static str,
    press: Value,
    release: Value,
) -> Result<()> {
    dispatch_pair_before(session, context_lease, method, press, release, None, None).await
}

async fn dispatch_pair_before(
    session: &Session,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
    method: &'static str,
    press: Value,
    release: Value,
    deadline: Option<Instant>,
    element: Option<Element>,
) -> Result<()> {
    // Bound each attempt separately so a stalled press still reaches release,
    // including when the caller has disabled command timeouts.
    let limit = Duration::from_secs(30);
    let timeout = session
        .connection()
        .command_timeout()
        .unwrap_or(limit)
        .min(limit);
    let session = session.clone().with_command_timeout(Some(timeout));
    session
        .connection()
        .runtime()
        .spawn(async move {
            // Check inside the owner, not just before spawning it. Once pressing
            // starts, even an uncertain/failed result must still reach release.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::Timeout("input deadline expired before press".into()));
            }
            let pressed = if let Some(deadline) = deadline {
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(deadline) => Err(Error::Timeout("input deadline expired during press".into())),
                    result = session.execute_raw(method, press) => result,
                }
            } else {
                session.execute_raw(method, press).await
            };
            // Release uses the original bounded handle timeout, not the now
            // possibly exhausted action budget. Never retry the press.
            let released = session.execute_raw(method, release).await;
            drop(element);
            // Session ownership alone does not retain a fallback browser context.
            drop(context_lease);
            with_cleanup(pressed.map(|_| ()), released.map(|_| ()))
        })
        .await?
}

impl Page {
    /// Click a point in CSS pixels relative to the target's viewport.
    ///
    /// [`ClickMethod::Cdp`] sends a left-button press/release pair, without a
    /// preceding mouse move. [`ClickMethod::JavaScript`] finds the node with
    /// `document.elementFromPoint` and dispatches an untrusted, bubbling,
    /// cancelable `MouseEvent("click")` in the isolated world. It does not invoke
    /// the node's `click()` method or send pointer input; cancellation of the
    /// synthetic event is not reported as an error. No node at the point is a
    /// JavaScript error. Neither method waits for navigation or retries input.
    pub async fn click_position(&self, x: f64, y: f64, method: ClickMethod) -> Result<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidArgument(
                "click coordinates must be finite".into(),
            ));
        }
        match method {
            ClickMethod::Cdp => click_cdp(&self.session, self.context_lease.clone(), x, y).await,
            ClickMethod::JavaScript => {
                self.evaluate(format!("(() => {{ const e = document.elementFromPoint({x},{y}); if (!e) throw new Error('No element at point'); e.dispatchEvent(new MouseEvent('click', {{bubbles:true,cancelable:true,clientX:{x},clientY:{y}}})); }})()")).world(super::EvaluationWorld::Isolated).run().await?;
                Ok(())
            }
        }
    }

    /// Schedule a diagnostic flash centered at viewport CSS-pixel coordinates.
    /// `size` is its initial diameter in CSS pixels and must be positive and finite;
    /// coordinates must also be finite. Completion means the animation and removal
    /// timer were scheduled, not that the animation finished or the marker vanished.
    /// This is a visual diagnostic, not a readiness or interaction barrier.
    ///
    /// Duration is rounded down to whole milliseconds, including zero for sub-ms
    /// values. Values exceeding `i32::MAX` whole milliseconds return
    /// [`Error::InvalidArgument`] before JavaScript runs, avoiding timer overflow.
    pub async fn flash_point(&self, x: f64, y: f64, duration: Duration, size: f64) -> Result<()> {
        if !x.is_finite() || !y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Err(Error::InvalidArgument("invalid flash geometry".into()));
        }
        let millis = duration.as_millis();
        if millis > i32::MAX as u128 {
            return Err(Error::InvalidArgument(
                "flash duration exceeds i32::MAX milliseconds".into(),
            ));
        }
        self.evaluate(format!("(() => {{ const d = document.createElement('div'); Object.assign(d.style, {{position:'fixed',pointerEvents:'none',zIndex:2147483647,left:'{}px',top:'{}px',width:'{size}px',height:'{size}px',borderRadius:'50%',background:'red'}}); document.documentElement.appendChild(d); d.animate([{{opacity:1,transform:'scale(1)'}},{{opacity:0,transform:'scale(3)'}}],{{duration:{millis}}}); setTimeout(() => d.remove(), {millis}); }})()",x-size/2.0,y-size/2.0)).world(super::EvaluationWorld::Isolated).run().await?;
        Ok(())
    }
}
