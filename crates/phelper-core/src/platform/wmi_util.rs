//! Shared WMI query helper (root\cimv2 typed queries). Hoisted from
//! identity.rs so OGH watch and future probes share one deserialize path.

use phelper_domain::error::EngineError;

/// wmi 0.18: exec_query yields Result<IWbemClassWrapper>; typed structs come
/// from per-item into_desr(). `what` labels errors for diagnostics.
pub(crate) fn query_typed<T: serde::de::DeserializeOwned>(
    conn: &wmi::WMIConnection,
    what: &str,
    query: &str,
) -> Result<Vec<T>, EngineError> {
    let iter = conn
        .exec_query(query)
        .map_err(|e| EngineError::WmiUnavailable(format!("{what}: {query}: {e}")))?;
    let mut out = Vec::new();
    for item in iter {
        let wrapper =
            item.map_err(|e| EngineError::WmiUnavailable(format!("{what}: {query} item: {e}")))?;
        out.push(
            wrapper
                .into_desr()
                .map_err(|e| EngineError::WmiUnavailable(format!("{what}: {query} deser: {e}")))?,
        );
    }
    Ok(out)
}
