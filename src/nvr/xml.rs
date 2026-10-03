use std::borrow::Cow;

use quick_xml::events::Event;

/// quick-xml emits entity references separately from UTF-8 text.
/// Resolve only predefined and numeric references, rejecting unknown entities.
pub(super) fn text<'a>(event: &'a Event<'_>) -> Result<Cow<'a, str>, quick_xml::Error> {
    match event {
        Event::Text(text) => Ok(Cow::Borrowed(text.as_ref())),
        Event::GeneralRef(reference) => {
            let escaped = format!("&{};", reference.as_ref());
            Ok(Cow::Owned(
                quick_xml::escape::unescape(&escaped)?.into_owned(),
            ))
        }
        _ => unreachable!("text is only called for text and entity references"),
    }
}
