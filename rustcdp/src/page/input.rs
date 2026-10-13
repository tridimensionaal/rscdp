use super::{ClickMethod, Element, Page};
use crate::{Error, Result, Session, cdp::input::commands::InsertText, error::with_cleanup};
use serde_json::{Value, json};
use std::time::Duration;

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
        dispatch_pair(&self.session, "Input.dispatchKeyEvent", down, up).await
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

    /// Focus this element and insert text using Chrome's input pipeline.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        self.focus().await?;
        insert_text(&self.0.session, text).await
    }
}

pub(super) async fn click_cdp(session: &Session, x: f64, y: f64) -> Result<()> {
    if !x.is_finite() || !y.is_finite() {
        return Err(Error::InvalidArgument(
            "click coordinates must be finite".into(),
        ));
    }
    dispatch_pair(
        session,
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
    )
    .await
}

async fn dispatch_pair(
    session: &Session,
    method: &'static str,
    press: Value,
    release: Value,
) -> Result<()> {
    let session = session.clone();
    session
        .connection()
        .runtime()
        .spawn(async move {
            let pressed = session.execute_raw(method, press).await;
            let released = session.execute_raw(method, release).await;
            with_cleanup(pressed.map(|_| ()), released.map(|_| ()))
        })
        .await?
}

impl Page {
    pub async fn click_position(&self, x: f64, y: f64, method: ClickMethod) -> Result<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidArgument(
                "click coordinates must be finite".into(),
            ));
        }
        match method {
            ClickMethod::Cdp => click_cdp(&self.session, x, y).await,
            ClickMethod::JavaScript => {
                self.evaluate_remote(&format!("(() => {{ const e = document.elementFromPoint({x},{y}); if (!e) throw new Error('No element at point'); e.dispatchEvent(new MouseEvent('click', {{bubbles:true,cancelable:true,clientX:{x},clientY:{y}}})); }})()"),true).await?;
                Ok(())
            }
        }
    }

    pub async fn press_escape(&self) -> Result<()> {
        self.press_key("Escape").await
    }

    pub async fn flash_point(&self, x: f64, y: f64, duration: Duration, size: f64) -> Result<()> {
        if !x.is_finite() || !y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Err(Error::InvalidArgument("invalid flash geometry".into()));
        }
        let millis = duration.as_millis();
        self.evaluate_remote(&format!("(() => {{ const d = document.createElement('div'); Object.assign(d.style, {{position:'fixed',pointerEvents:'none',zIndex:2147483647,left:'{}px',top:'{}px',width:'{size}px',height:'{size}px',borderRadius:'50%',background:'red'}}); document.documentElement.appendChild(d); d.animate([{{opacity:1,transform:'scale(1)'}},{{opacity:0,transform:'scale(3)'}}],{{duration:{millis}}}); setTimeout(() => d.remove(), {millis}); }})()",x-size/2.0,y-size/2.0),true).await?;
        Ok(())
    }
}
