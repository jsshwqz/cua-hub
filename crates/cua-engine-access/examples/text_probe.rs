// Probe which text interfaces the focused control actually exposes, and what
// they return. Used to tell "this app has no selectable text" apart from "our
// Text pattern wiring is wrong".
//
//   cargo run -p cua-engine-access --example text_probe
fn main() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
        };
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
        };

        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.expect("uia");
        let focused = unsafe { automation.GetFocusedElement() }.expect("focused");
        let name = unsafe { focused.CurrentName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let ctype = unsafe { focused.CurrentControlType() }
            .map(|c| c.0)
            .unwrap_or_default();
        println!("focused: type={ctype} name={name:?}");

        match unsafe { focused.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) }
        {
            Ok(pattern) => {
                println!("TextPattern: available");
                match unsafe { pattern.GetSelection() } {
                    Ok(selection) => {
                        let len = unsafe { selection.Length() }.unwrap_or(-1);
                        println!("  GetSelection length: {len}");
                        if len > 0 {
                            let range = unsafe { selection.GetElement(0) }.expect("range");
                            let text = unsafe { range.GetText(200) }
                                .map(|b| b.to_string())
                                .unwrap_or_default();
                            println!("  selection text: {text:?}");
                        }
                    }
                    Err(e) => println!("  GetSelection failed: {e}"),
                }
                match unsafe { pattern.DocumentRange() } {
                    Ok(doc) => {
                        let text = unsafe { doc.GetText(200) }
                            .map(|b| b.to_string())
                            .unwrap_or_default();
                        println!("  document text: {text:?}");
                    }
                    Err(e) => println!("  DocumentRange failed: {e}"),
                }
            }
            Err(e) => println!("TextPattern: unavailable ({e})"),
        }

        // Also report the value pattern, which is what the tree reads.
        use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_ValuePatternId};
        match unsafe {
            focused.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        } {
            Ok(value) => {
                let text = unsafe { value.CurrentValue() }
                    .map(|b| b.to_string())
                    .unwrap_or_default();
                println!("ValuePattern: {:?}", text);
            }
            Err(e) => println!("ValuePattern: unavailable ({e})"),
        }
    }
    #[cfg(not(windows))]
    println!("text probe is windows-only");
}
