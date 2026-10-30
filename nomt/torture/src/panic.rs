/// Panics are caught with `std::panic::catch_unwind` which returns an `std::thread::Result`,
/// the variant `Err` will contain a `Box<dyn Any + Send>` error from which it is possible
/// to extract an error message. Those utilities allow to handle those panic error messages.
use std::any::Any;

/// Attempt to create a `String` with the given context and downcast
/// the error to look for a message within it. If no message is found,
/// the `String` will contain only the context.
pub fn panic_to_string(context: &str, err: Box<dyn Any + Send>) -> String {
    if let Some(err) = err.downcast_ref::<&str>() {
        return format!("{}: {}", context, err);
    }
    if let Some(err) = err.downcast_ref::<String>() {
        return format!("{}: {}", context, err);
    }
    format!("{} (no message)", context)
}

/// Creates a `anyhow::Result::Err(..)` from a context and an error
/// possibly containing a message.
pub fn panic_to_err<T>(context: &str, err: Box<dyn Any + Send>) -> anyhow::Result<T> {
    Err(anyhow::anyhow!("{}", panic_to_string(context, err)))
}
