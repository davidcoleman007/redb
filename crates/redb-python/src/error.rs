use pyo3::PyErrArguments;
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyTuple, PyType};

create_exception!(
    redb,
    Error,
    PyException,
    "The base of every error redb raises."
);

// Raised by the bindings themselves
create_exception!(
    redb,
    TransactionCompleted,
    Error,
    "The transaction was already committed or aborted."
);

create_exception!(
    redb,
    UsedAfterFork,
    Error,
    "Opened by the process this one was forked from."
);

/// An exception class that derives from a builtin exception as well as from `Error`, which
/// `create_exception!`, taking one base, cannot make
pub(crate) struct Subclass {
    name: &'static str,
    doc: &'static str,
    builtin: fn(Python<'_>) -> Bound<'_, PyType>,
    class: PyOnceLock<Py<PyType>>,
}

impl Subclass {
    const fn new(
        name: &'static str,
        doc: &'static str,
        builtin: fn(Python<'_>) -> Bound<'_, PyType>,
    ) -> Self {
        Self {
            name,
            doc,
            builtin,
            class: PyOnceLock::new(),
        }
    }

    fn class<'py>(&self, py: Python<'py>) -> PyResult<&Bound<'py, PyType>> {
        let class = self.class.get_or_try_init(py, || -> PyResult<_> {
            let attributes = PyDict::new(py);
            attributes.set_item("__module__", "redb")?;
            attributes.set_item("__doc__", self.doc)?;
            let bases = PyTuple::new(py, [py.get_type::<Error>(), (self.builtin)(py)])?;
            Ok(py
                .get_type::<PyType>()
                .call1((self.name, bases, attributes))?
                .cast_into::<PyType>()?
                .unbind())
        })?;
        Ok(class.bind(py))
    }

    pub(crate) fn new_err<A: PyErrArguments + Send + Sync + 'static>(&self, args: A) -> PyErr {
        Python::attach(|py| match self.class(py) {
            Ok(class) => PyErr::from_type(class.clone(), args),
            Err(err) => err,
        })
    }
}

/// `redb::Error::Io`, an `OSError` as well, with the OS error number where there is one
pub(crate) static IO: Subclass =
    Subclass::new("Io", "An I/O error.", |py| py.get_type::<PyOSError>());

// Raised by the bindings themselves
pub(crate) static TABLE_CHANGED_DURING_ITERATION: Subclass = Subclass::new(
    "TableChangedDuringIteration",
    "The table changed while a range over it was being iterated.",
    |py| py.get_type::<PyRuntimeError>(),
);
pub(crate) static TRANSACTION_IN_USE: Subclass = Subclass::new(
    "TransactionInUse",
    "The transaction is in use by another operation, such as the predicate of a retain().",
    |py| py.get_type::<PyRuntimeError>(),
);

fn into_py_err(err: ::redb::Error) -> PyErr {
    let msg = err.to_string();
    match &err {
        ::redb::Error::Io(io) => match io.raw_os_error() {
            Some(errno) => IO.new_err((errno, msg)),
            None => IO.new_err((msg,)),
        },
        _ => variant_into_py_err(err, msg),
    }
}

// One exception class per `redb::Error` variant, each a direct subclass of `Error`, but for
// `Io`, above. Every other redb error enum converts into `redb::Error`, which flattens them all.
macro_rules! variants {
    ($($name:ident),* $(,)?) => {
        $(create_exception!(redb, $name, Error);)*

        fn variant_into_py_err(err: ::redb::Error, msg: String) -> PyErr {
            match err {
                $(::redb::Error::$name { .. } => $name::new_err(msg),)*
                // redb::Error is #[non_exhaustive]
                _ => Error::new_err(msg),
            }
        }

        fn add_variants(m: &Bound<'_, PyModule>) -> PyResult<()> {
            $(m.add(stringify!($name), m.py().get_type::<$name>())?;)*
            Ok(())
        }
    };
}

variants! {
    DatabaseAlreadyOpen,
    LockProtocolMismatch,
    InvalidSavepoint,
    ImmediateDurabilityRequired,
    RepairAborted,
    PersistentSavepointModified,
    NonDurableCommitUnsupported,
    EphemeralSavepointUnsupported,
    PersistentSavepointExists,
    EphemeralSavepointExists,
    TransactionInProgress,
    TransactionPoisoned,
    Corrupted,
    UpgradeRequired,
    ValueTooLarge,
    UnorderedKey,
    TableTypeMismatch,
    TableIsMultimap,
    TableIsNotMultimap,
    TypeDefinitionChanged,
    TableDoesNotExist,
    TableExists,
    TableAlreadyOpen,
    Unsupported,
    DatabaseClosed,
    PreviousIo,
    LockPoisoned,
    ReadTransactionStillInUse,
}

// `create_exception!` types are not `#[pyclass]`es, so instead of `add_class` each class is
// stored as a module attribute, which is all `redb.Io` is.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("Error", py.get_type::<Error>())?;
    m.add(
        "TransactionCompleted",
        py.get_type::<TransactionCompleted>(),
    )?;
    m.add("UsedAfterFork", py.get_type::<UsedAfterFork>())?;
    for subclass in [&IO, &TABLE_CHANGED_DURING_ITERATION, &TRANSACTION_IN_USE] {
        m.add(subclass.name, subclass.class(py)?)?;
    }
    add_variants(m)
}

/// `result.pyerr()?` raises a redb error as its exception
pub(crate) trait PyErrExt<T> {
    fn pyerr(self) -> PyResult<T>;
}

impl<T, E: Into<::redb::Error>> PyErrExt<T> for Result<T, E> {
    fn pyerr(self) -> PyResult<T> {
        self.map_err(|err| into_py_err(err.into()))
    }
}
