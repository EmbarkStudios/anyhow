use crate::context::ContextError;
use std::any::TypeId;
use std::error::Error as StdError;
use std::fmt::{self, Debug, Display};
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr;

#[cfg(backtrace)]
use std::backtrace::{Backtrace, BacktraceStatus};

/// The `Error` type, a wrapper around a dynamic error type.
///
/// `Error` works a lot like `Box<dyn std::error::Error>`, but with these
/// differences:
///
/// - `Error` requires that the error is `Send`, `Sync`, and `'static`.
/// - `Error` guarantees that a backtrace is available, even if the underlying
///   error type does not provide one.
/// - `Error` is represented as a narrow pointer &mdash; exactly one word in
///   size instead of two.
pub struct Error {
    inner: Box<ErrorImpl<()>>,
}

impl Error {
    /// Create a new error object from any error type.
    ///
    /// The error type must be threadsafe and `'static`, so that the `Error`
    /// will be as well.
    ///
    /// If the error type does not provide a backtrace, a backtrace will be
    /// created here to ensure that a backtrace exists.
    pub fn new<E>(error: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        // Captured here instead of in Error::construct to have one fewer layer
        // of wrapping visible in the backtrace.
        #[cfg(backtrace)]
        let backtrace = match error.backtrace() {
            Some(_) => None,
            None => Some(Backtrace::capture()),
        };

        #[cfg(not(backtrace))]
        let backtrace = None;

        Error::construct(error, TypeId::of::<E>(), backtrace)
    }

    pub(crate) fn new_adhoc<M>(message: M, backtrace: Option<Backtrace>) -> Self
    where
        M: Display + Debug + Send + Sync + 'static,
    {
        Error::construct(MessageError(message), TypeId::of::<M>(), backtrace)
    }

    fn construct<E>(error: E, type_id: TypeId, backtrace: Option<Backtrace>) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        unsafe {
            let obj = mem::transmute::<&dyn StdError, TraitObject>(&error);
            let inner = Box::new(ErrorImpl {
                vtable: obj.vtable,
                type_id,
                backtrace,
                error,
            });
            Error {
                inner: mem::transmute::<Box<ErrorImpl<E>>, Box<ErrorImpl<()>>>(inner),
            }
        }
    }

    /// Wrap the error value with additional context.
    ///
    /// For attaching context to a `Result` as it is propagated, the
    /// [`Context`][crate::Context] extension trait may be more convenient than
    /// this function.
    ///
    /// The primary reason to use `error.context(...)` instead of
    /// `result.context(...)` via the `Context` trait would be if the context
    /// needs to depend on some data held by the underlying error:
    ///
    /// ```
    /// # use std::fmt::{self, Debug, Display};
    /// #
    /// # type T = ();
    /// #
    /// # impl std::error::Error for ParseError {}
    /// # impl Debug for ParseError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// # impl Display for ParseError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// #
    /// use std::fs::File;
    /// use std::path::Path;
    ///
    /// struct ParseError {
    ///     line: usize,
    ///     column: usize,
    /// }
    ///
    /// fn parse_impl(file: File) -> Result<T, ParseError> {
    ///     # const IGNORE: &str = stringify! {
    ///     ...
    ///     # };
    ///     # unimplemented!()
    /// }
    ///
    /// pub fn parse(path: impl AsRef<Path>) -> anyhow::Result<T> {
    ///     let file = File::open(&path)?;
    ///     parse_impl(file).map_err(|error| {
    ///         let context = format!(
    ///             "only the first {} lines of {} are valid",
    ///             error.line, path.as_ref().display(),
    ///         );
    ///         anyhow::Error::new(error).context(context)
    ///     })
    /// }
    /// ```
    pub fn context<C>(self, context: C) -> Self
    where
        C: Display + Send + Sync + 'static,
    {
        Error::from(ContextError {
            error: self,
            context,
        })
    }

    /// Get the backtrace for this Error.
    ///
    /// Backtraces are only available on the nightly channel. Tracking issue:
    /// [rust-lang/rust#53487][tracking].
    ///
    /// In order for the backtrace to be meaningful, the environment variable
    /// `RUST_LIB_BACKTRACE=1` must be defined. Backtraces are somewhat
    /// expensive to capture in Rust, so we don't necessarily want to be
    /// capturing them all over the place all the time.
    ///
    /// [tracking]: https://github.com/rust-lang/rust/issues/53487
    #[cfg(backtrace)]
    pub fn backtrace(&self) -> &Backtrace {
        // NB: this unwrap can only fail if the underlying error's backtrace
        // method is nondeterministic, which would only happen in maliciously
        // constructed code
        self.inner
            .backtrace
            .as_ref()
            .or_else(|| self.inner.error().backtrace())
            .expect("backtrace capture failed")
    }

    /// An iterator of the chain of source errors contained by this Error.
    ///
    /// This iterator will visit every error in the cause chain of this error
    /// object, beginning with the error that this error object was created
    /// from.
    ///
    /// # Example
    ///
    /// ```
    /// use anyhow::Error;
    /// use std::io;
    ///
    /// pub fn underlying_io_error_kind(error: &Error) -> Option<io::ErrorKind> {
    ///     for cause in error.chain() {
    ///         if let Some(io_error) = cause.downcast_ref::<io::Error>() {
    ///             return Some(io_error.kind());
    ///         }
    ///     }
    ///     None
    /// }
    /// ```
    pub fn chain(&self) -> Chain {
        Chain {
            next: Some(self.inner.error()),
        }
    }

    /// The lowest level cause of this error &mdash; this error's cause's
    /// cause's cause etc.
    ///
    /// The root cause is the last error in the iterator produced by
    /// [`chain()`][Error::chain].
    pub fn root_cause(&self) -> &(dyn StdError + 'static) {
        let mut chain = self.chain();
        let mut root_cause = chain.next().unwrap();
        for cause in chain {
            root_cause = cause;
        }
        root_cause
    }

    /// Returns `true` if `E` is the type wrapped by this error object.
    pub fn is<E>(&self) -> bool
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        TypeId::of::<E>() == self.inner.type_id
    }

    /// Attempt to downcast the error object to a concrete type.
    pub fn downcast<E>(self) -> Result<E, Self>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        if let Some(error) = self.downcast_ref::<E>() {
            unsafe {
                let error = ptr::read(error);
                drop(ptr::read(&self.inner));
                mem::forget(self);
                Ok(error)
            }
        } else {
            Err(self)
        }
    }

    /// Downcast this error object by reference.
    ///
    /// # Example
    ///
    /// ```
    /// # use anyhow::anyhow;
    /// # use std::fmt::{self, Display};
    /// # use std::task::Poll;
    /// #
    /// # #[derive(Debug)]
    /// # enum DataStoreError {
    /// #     Censored(()),
    /// # }
    /// #
    /// # impl Display for DataStoreError {
    /// #     fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    /// #         unimplemented!()
    /// #     }
    /// # }
    /// #
    /// # impl std::error::Error for DataStoreError {}
    /// #
    /// # const REDACTED_CONTENT: () = ();
    /// #
    /// # let error = anyhow!("...");
    /// # let root_cause = &error;
    /// #
    /// # let ret =
    /// // If the error was caused by redaction, then return a tombstone instead
    /// // of the content.
    /// match root_cause.downcast_ref::<DataStoreError>() {
    ///     Some(DataStoreError::Censored(_)) => Ok(Poll::Ready(REDACTED_CONTENT)),
    ///     None => Err(error),
    /// }
    /// # ;
    /// ```
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        if self.is::<E>() {
            unsafe { Some(&*(self.inner.error() as *const dyn StdError as *const E)) }
        } else {
            None
        }
    }

    /// Downcast this error object by mutable reference.
    pub fn downcast_mut<E>(&mut self) -> Option<&mut E>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        if self.is::<E>() {
            unsafe { Some(&mut *(self.inner.error_mut() as *mut dyn StdError as *mut E)) }
        } else {
            None
        }
    }
}

#[cfg(not(backtrace))]
pub(crate) enum Backtrace {}

impl<E> From<E> for Error
where
    E: StdError + Send + Sync + 'static,
{
    fn from(error: E) -> Self {
        Error::new(error)
    }
}

impl Deref for Error {
    type Target = dyn StdError + Send + Sync + 'static;

    fn deref(&self) -> &Self::Target {
        self.inner.error()
    }
}

impl DerefMut for Error {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.error_mut()
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "{}", self.inner.error())?;

        let mut chain = self.chain().skip(1).enumerate().peekable();
        if let Some((n, error)) = chain.next() {
            write!(f, "\nCaused by:\n    ")?;
            if chain.peek().is_some() {
                write!(f, "{}: ", n)?;
            }
            writeln!(f, "{}", error)?;
            for (n, error) in chain {
                writeln!(f, "    {}: {}", n, error)?;
            }
        }

        #[cfg(backtrace)]
        {
            let backtrace = self.backtrace();
            match backtrace.status() {
                BacktraceStatus::Captured => {
                    writeln!(f, "\n{}", backtrace)?;
                }
                BacktraceStatus::Disabled => {
                    writeln!(
                        f,
                        "\nBacktrace disabled; run with RUST_LIB_BACKTRACE=1 environment variable to display a backtrace"
                    )?;
                }
                _ => {}
            }
        }

        Ok(())
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.inner.error())
    }
}

unsafe impl Send for Error {}
unsafe impl Sync for Error {}

impl Drop for Error {
    fn drop(&mut self) {
        unsafe { ptr::drop_in_place(self.inner.error_mut()) }
    }
}

// repr C to ensure that `E` remains in the final position
#[repr(C)]
struct ErrorImpl<E> {
    vtable: *const (),
    type_id: TypeId,
    backtrace: Option<Backtrace>,
    error: E,
}

// repr C to ensure that transmuting from trait objects is safe
#[repr(C)]
struct TraitObject {
    data: *const (),
    vtable: *const (),
}

#[repr(transparent)]
struct MessageError<M>(M);

impl<M> Debug for MessageError<M>
where
    M: Display + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<M> Display for MessageError<M>
where
    M: Display + Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<M> StdError for MessageError<M> where M: Display + Debug + 'static {}

impl ErrorImpl<()> {
    fn error(&self) -> &(dyn StdError + Send + Sync + 'static) {
        let object = TraitObject {
            data: &self.error,
            vtable: self.vtable,
        };

        unsafe { mem::transmute::<TraitObject, &(dyn StdError + Send + Sync + 'static)>(object) }
    }

    fn error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
        let object = TraitObject {
            data: &mut self.error,
            vtable: self.vtable,
        };

        unsafe {
            mem::transmute::<TraitObject, &mut (dyn StdError + Send + Sync + 'static)>(object)
        }
    }
}

/// Iterator of a chain of source errors.
///
/// This type is the iterator returned by [`Error::chain`].
///
/// # Example
///
/// ```
/// use anyhow::Error;
/// use std::io;
///
/// pub fn underlying_io_error_kind(error: &Error) -> Option<io::ErrorKind> {
///     for cause in error.chain() {
///         if let Some(io_error) = cause.downcast_ref::<io::Error>() {
///             return Some(io_error.kind());
///         }
///     }
///     None
/// }
/// ```
pub struct Chain<'a> {
    next: Option<&'a (dyn StdError + 'static)>,
}

impl<'a> Iterator for Chain<'a> {
    type Item = &'a (dyn StdError + 'static);

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.next.take()?;
        self.next = next.source();
        Some(next)
    }
}

#[cfg(test)]
mod repr_correctness {
    use super::*;
    use std::marker::Unpin;
    use std::mem;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Arc;

    #[test]
    fn size_of_error() {
        assert_eq!(mem::size_of::<Error>(), mem::size_of::<usize>());
    }

    #[test]
    fn error_autotraits() {
        fn assert<E: Unpin + Send + Sync + 'static>() {}
        assert::<Error>();
    }

    #[test]
    fn drop_works() {
        #[derive(Debug)]
        struct DetectDrop {
            has_dropped: Arc<AtomicBool>,
        }

        impl StdError for DetectDrop {}

        impl Display for DetectDrop {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "does something")
            }
        }

        impl Drop for DetectDrop {
            fn drop(&mut self) {
                let already_dropped = self.has_dropped.swap(true, SeqCst);
                assert!(!already_dropped);
            }
        }

        let has_dropped = Arc::new(AtomicBool::new(false));

        drop(Error::from(DetectDrop {
            has_dropped: has_dropped.clone(),
        }));

        assert!(has_dropped.load(SeqCst));
    }
}
