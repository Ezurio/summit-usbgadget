//
// SPDX-License-Identifier: MIT OR Apache-2.0
//

use std::any::Any;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::de::DeserializeOwned;
use toml::Table;
use toml::Value;
use usb_gadget::function::Handle;

#[doc(hidden)]
pub use inventory::submit as __inventory_submit;

pub trait RegisteredFunctionConfig: fmt::Debug + Send + Sync {
    fn kind(&self) -> &'static str;
    fn build(&self, serial: &str, context: &mut FunctionBuildContext) -> Result<Option<Handle>, Box<dyn Error>>;
    fn clone_box(&self) -> Box<dyn RegisteredFunctionConfig>;
    fn as_any(&self) -> &dyn Any;
}

impl Clone for Box<dyn RegisteredFunctionConfig> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

pub trait GadgetService: Send {
    fn spawn(self: Box<Self>, udc_name: String) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

#[derive(Default)]
pub struct FunctionBuildContext {
    claimed_singletons: HashSet<&'static str>,
    services: Vec<Box<dyn GadgetService>>,
}

impl FunctionBuildContext {
    pub fn claim_singleton(&mut self, kind: &'static str) -> Result<(), Box<dyn Error>> {
        if !self.claimed_singletons.insert(kind) {
            return Err(format!("configuration declares more than one {kind} function").into());
        }
        Ok(())
    }

    pub fn push_service<S>(&mut self, service: S)
    where
        S: GadgetService + 'static,
    {
        self.services.push(Box::new(service));
    }

    pub fn into_services(self) -> Vec<Box<dyn GadgetService>> {
        self.services
    }
}

pub struct FunctionRegistration {
    pub kind: &'static str,
    pub parse: fn(Table) -> Result<Box<dyn RegisteredFunctionConfig>, String>,
}

impl FunctionRegistration {
    pub const fn new(
        kind: &'static str,
        parse: fn(Table) -> Result<Box<dyn RegisteredFunctionConfig>, String>,
    ) -> Self {
        Self { kind, parse }
    }
}

inventory::collect!(FunctionRegistration);

pub fn parse_registered_function<T>(table: Table) -> Result<Box<dyn RegisteredFunctionConfig>, String>
where
    T: RegisteredFunctionConfig + DeserializeOwned + 'static,
{
    let config: T = Value::Table(table).try_into().map_err(|err: toml::de::Error| err.to_string())?;
    Ok(Box::new(config))
}

pub fn parse_function(kind: &str, table: Table) -> Result<Option<Box<dyn RegisteredFunctionConfig>>, String> {
    let mut registrations = inventory::iter::<FunctionRegistration>
        .into_iter()
        .filter(|registration| registration.kind == kind);
    let Some(registration) = registrations.next() else {
        return Ok(None);
    };
    if registrations.next().is_some() {
        return Err(format!("multiple USB function registrations found for type {kind:?}"));
    }
    (registration.parse)(table).map(Some)
}

#[macro_export]
macro_rules! declare_usb_function {
    ($kind:literal => $ty:ty) => {
        $crate::__inventory_submit! {
            $crate::FunctionRegistration::new($kind, $crate::parse_registered_function::<$ty>)
        }
    };
}