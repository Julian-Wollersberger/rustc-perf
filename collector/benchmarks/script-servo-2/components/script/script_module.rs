/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The script module mod contains common traits and structs
//! related to `type=module` for script thread or worker threads.

use crate::document_loader::LoadType;
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowBinding::WindowMethods;
use crate::dom::bindings::conversions::jsstring_to_str;
use crate::dom::bindings::error::report_pending_exception;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomObject;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::settings_stack::AutoIncumbentScript;
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::globalscope::GlobalScope;
use crate::dom::htmlscriptelement::{HTMLScriptElement, ScriptId};
use crate::dom::htmlscriptelement::{ScriptOrigin, ScriptType, SCRIPT_JS_MIMES};
use crate::dom::node::document_from_node;
use crate::dom::performanceresourcetiming::InitiatorType;
use crate::dom::promise::Promise;
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::dom::window::Window;
use crate::dom::worker::TrustedWorkerAddress;
use crate::network_listener::{self, NetworkListener};
use crate::network_listener::{PreInvoke, ResourceTimingListener};
use crate::realms::{enter_realm, AlreadyInRealm, InRealm};
use crate::script_runtime::JSContext as SafeJSContext;
use crate::task::TaskBox;
use crate::task_source::TaskSourceName;
use encoding_rs::UTF_8;
use hyper_serde::Serde;
use indexmap::{IndexMap, IndexSet};
use ipc_channel::ipc;
use ipc_channel::router::ROUTER;
use js::jsapi::Handle as RawHandle;
use js::jsapi::HandleObject;
use js::jsapi::HandleValue as RawHandleValue;
use js::jsapi::{CompileModule, ExceptionStackBehavior};
use js::jsapi::{GetModuleResolveHook, JSRuntime, SetModuleResolveHook};
use js::jsapi::{GetRequestedModules, SetModuleMetadataHook};
use js::jsapi::{Heap, JSContext, JS_ClearPendingException, SetModulePrivate};
use js::jsapi::{JSAutoRealm, JSObject, JSString};
use js::jsapi::{JS_DefineProperty4, JS_NewStringCopyN, JSPROP_ENUMERATE};
use js::jsapi::{ModuleEvaluate, ModuleInstantiate};
use js::jsapi::{SetModuleDynamicImportHook, SetScriptPrivateReferenceHooks};
use js::jsval::{JSVal, PrivateValue, UndefinedValue};
use js::rust::jsapi_wrapped::{GetRequestedModuleSpecifier, JS_GetPendingException};
use js::rust::jsapi_wrapped::{JS_GetArrayLength, JS_GetElement};
use js::rust::transform_u16_to_source_text;
use js::rust::wrappers::JS_SetPendingException;
use js::rust::CompileOptionsWrapper;
use js::rust::{Handle, HandleValue, IntoHandle};
use mime::Mime;
use net_traits::request::{CredentialsMode, Destination, ParserMetadata};
use net_traits::request::{Referrer, RequestBuilder, RequestMode};
use net_traits::{FetchMetadata, Metadata};
use net_traits::{FetchResponseListener, NetworkError};
use net_traits::{ResourceFetchTiming, ResourceTimingType};
use servo_url::ServoUrl;
use std::collections::HashSet;
use std::ffi;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use url::ParseError as UrlParseError;

#[allow(unsafe_code)]
unsafe fn gen_type_error(global: &GlobalScope, string: String) -> RethrowError {
    rooted!(in(*global.get_cx()) let mut thrown = UndefinedValue());
    Error::Type(string).to_jsval(*global.get_cx(), &global, thrown.handle_mut());

    return RethrowError(RootedTraceableBox::from_box(Heap::boxed(thrown.get())));
}

#[derive(JSTraceable)]
pub struct ModuleObject(Box<Heap<*mut JSObject>>);

impl ModuleObject {
    #[allow(unsafe_code)]
    pub fn handle(&self) -> HandleObject {
        unsafe { self.0.handle() }
    }
}

#[derive(JSTraceable)]
pub struct RethrowError(RootedTraceableBox<Heap<JSVal>>);

impl RethrowError {
    fn handle(&self) -> Handle<JSVal> {
        self.0.handle()
    }
}

impl Clone for RethrowError {
    fn clone(&self) -> Self {
        Self(RootedTraceableBox::from_box(Heap::boxed(
            self.0.get().clone(),
        )))
    }
}

struct ModuleScript {
    base_url: ServoUrl,
}

/// Identity for a module which will be
/// used to retrieve the module when we'd
/// like to get it from module map.
///
/// For example, we will save module parents with
/// module identity so that we can get module tree
/// from a descendant no matter the parent is an
/// inline script or a external script
#[derive(Clone, Eq, Hash, JSTraceable, PartialEq)]
pub enum ModuleIdentity {
    ScriptId(ScriptId),
    ModuleUrl(ServoUrl),
}

impl ModuleIdentity {
    pub fn get_module_tree(&self, global: &GlobalScope) -> Rc<ModuleTree> {
        match self {
            ModuleIdentity::ModuleUrl(url) => {
                let module_map = global.get_module_map().borrow();
                module_map.get(&url.clone()).unwrap().clone()
            },
            ModuleIdentity::ScriptId(script_id) => {
                let inline_module_map = global.get_inline_module_map().borrow();
                inline_module_map.get(&script_id).unwrap().clone()
            },
        }
    }
}

#[derive(JSTraceable)]
pub struct ModuleTree {
    url: ServoUrl,
    text: DomRefCell<DOMString>,
    record: DomRefCell<Option<ModuleObject>>,
    status: DomRefCell<ModuleStatus>,
    // The spec maintains load order for descendants, so we use an indexset for descendants and
    // parents. This isn't actually necessary for parents however the IndexSet APIs don't
    // interop with HashSet, and IndexSet isn't very expensive
    // (https://github.com/bluss/indexmap/issues/110)
    //
    // By default all maps in web specs are ordered maps
    // (https://infra.spec.whatwg.org/#ordered-map), however we can usually get away with using
    // stdlib maps and sets because we rarely iterate over them.
    parent_identities: DomRefCell<IndexSet<ModuleIdentity>>,
    descendant_urls: DomRefCell<IndexSet<ServoUrl>>,
    // A set to memoize which descendants are under fetching
    incomplete_fetch_urls: DomRefCell<IndexSet<ServoUrl>>,
    visited_urls: DomRefCell<HashSet<ServoUrl>>,
    rethrow_error: DomRefCell<Option<RethrowError>>,
    network_error: DomRefCell<Option<NetworkError>>,
    // A promise for owners to execute when the module tree
    // is finished
    promise: DomRefCell<Option<Rc<Promise>>>,
    external: bool,
}

impl ModuleTree {
    pub fn new(url: ServoUrl, external: bool, visited_urls: HashSet<ServoUrl>) -> Self {
        ModuleTree {
            url,
            text: DomRefCell::new(DOMString::new()),
            record: DomRefCell::new(None),
            status: DomRefCell::new(ModuleStatus::Initial),
            parent_identities: DomRefCell::new(IndexSet::new()),
            descendant_urls: DomRefCell::new(IndexSet::new()),
            incomplete_fetch_urls: DomRefCell::new(IndexSet::new()),
            visited_urls: DomRefCell::new(visited_urls),
            rethrow_error: DomRefCell::new(None),
            network_error: DomRefCell::new(None),
            promise: DomRefCell::new(None),
            external,
        }
    }

    pub fn get_status(&self) -> ModuleStatus {
        self.status.borrow().clone()
    }

    pub fn set_status(&self, status: ModuleStatus) {
        *self.status.borrow_mut() = status;
    }

    pub fn get_record(&self) -> &DomRefCell<Option<ModuleObject>> {
        &self.record
    }

    pub fn set_record(&self, record: ModuleObject) {
        *self.record.borrow_mut() = Some(record);
    }

    pub fn get_rethrow_error(&self) -> &DomRefCell<Option<RethrowError>> {
        &self.rethrow_error
    }

    pub fn set_rethrow_error(&self, rethrow_error: RethrowError) {
        *self.rethrow_error.borrow_mut() = Some(rethrow_error);
    }

    pub fn get_network_error(&self) -> &DomRefCell<Option<NetworkError>> {
        &self.network_error
    }

    pub fn set_network_error(&self, network_error: NetworkError) {
        *self.network_error.borrow_mut() = Some(network_error);
    }

    pub fn get_text(&self) -> &DomRefCell<DOMString> {
        &self.text
    }

    pub fn set_text(&self, module_text: DOMString) {
        *self.text.borrow_mut() = module_text;
    }

    pub fn get_incomplete_fetch_urls(&self) -> &DomRefCell<IndexSet<ServoUrl>> {
        &self.incomplete_fetch_urls
    }

    pub fn get_descendant_urls(&self) -> &DomRefCell<IndexSet<ServoUrl>> {
        &self.descendant_urls
    }

    pub fn get_parent_urls(&self) -> IndexSet<ServoUrl> {
        let parent_identities = self.parent_identities.borrow();

        parent_identities
            .iter()
            .filter_map(|parent_identity| match parent_identity {
                ModuleIdentity::ScriptId(_) => None,
                ModuleIdentity::ModuleUrl(url) => Some(url.clone()),
            })
            .collect()
    }

    pub fn insert_parent_identity(&self, parent_identity: ModuleIdentity) {
        self.parent_identities.borrow_mut().insert(parent_identity);
    }

    pub fn insert_incomplete_fetch_url(&self, dependency: ServoUrl) {
        self.incomplete_fetch_urls.borrow_mut().insert(dependency);
    }

    pub fn remove_incomplete_fetch_url(&self, dependency: ServoUrl) {
        self.incomplete_fetch_urls.borrow_mut().remove(&dependency);
    }

    /// Find circular dependencies in non-recursive way
    ///
    /// This function is basically referred to
    /// [this blog post](https://breakingcode.wordpress.com/2013/03/11/an-example-dependency-resolution-algorithm-in-python/).
    ///
    /// The only difference is, in that blog post, its algorithm will throw errors while finding circular
    /// dependencies; however, in our use case, we'd like to find circular dependencies so we will just
    /// return it.
    pub fn find_circular_dependencies(&self, global: &GlobalScope) -> IndexSet<ServoUrl> {
        let module_map = global.get_module_map().borrow();

        // A map for checking dependencies and using the module url as key
        let mut module_deps: IndexMap<ServoUrl, IndexSet<ServoUrl>> = module_map
            .iter()
            .map(|(module_url, module)| {
                (module_url.clone(), module.descendant_urls.borrow().clone())
            })
            .collect();

        while module_deps.len() != 0 {
            // Get all dependencies with no dependencies
            let ready: IndexSet<ServoUrl> = module_deps
                .iter()
                .filter_map(|(module_url, descendant_urls)| {
                    if descendant_urls.len() == 0 {
                        Some(module_url.clone())
                    } else {
                        None
                    }
                })
                .collect();

            // If there's no ready module but we're still in the loop,
            // it means we find circular modules, then we can return them.
            if ready.len() == 0 {
                return module_deps
                    .iter()
                    .map(|(url, _)| url.clone())
                    .collect::<IndexSet<ServoUrl>>();
            }

            // Remove ready modules from the dependency map
            for module_url in ready.iter() {
                module_deps.remove(&module_url.clone());
            }

            // Also make sure to remove the ready modules from the
            // remaining module dependencies as well
            for (_, deps) in module_deps.iter_mut() {
                *deps = deps
                    .difference(&ready)
                    .into_iter()
                    .cloned()
                    .collect::<IndexSet<ServoUrl>>();
            }
        }

        IndexSet::new()
    }

    // We just leverage the power of Promise to run the task for `finish` the owner.
    // Thus, we will always `resolve` it and no need to register a callback for `reject`
    pub fn append_handler(&self, owner: ModuleOwner, module_identity: ModuleIdentity) {
        let this = owner.clone();
        let identity = module_identity.clone();

        let handler = PromiseNativeHandler::new(
            &owner.global(),
            Some(ModuleHandler::new(Box::new(
                task!(fetched_resolve: move || {
                    this.notify_owner_to_finish(identity);
                }),
            ))),
            None,
        );

        let realm = enter_realm(&*owner.global());
        let comp = InRealm::Entered(&realm);
        let _ais = AutoIncumbentScript::new(&*owner.global());

        let mut promise = self.promise.borrow_mut();
        match promise.as_ref() {
            Some(promise) => promise.append_native_handler(&handler, comp),
            None => {
                let new_promise = Promise::new_in_current_realm(&owner.global(), comp);
                new_promise.append_native_handler(&handler, comp);
                *promise = Some(new_promise);
            },
        }
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, PartialEq, PartialOrd)]
pub enum ModuleStatus {
    Initial,
    Fetching,
    FetchingDescendants,
    Finished,
}

impl ModuleTree {
    #[allow(unsafe_code)]
    /// https://html.spec.whatwg.org/multipage/#creating-a-module-script
    /// Step 7-11.
    fn compile_module_script(
        &self,
        global: &GlobalScope,
        module_script_text: DOMString,
        url: ServoUrl,
    ) -> Result<ModuleObject, RethrowError> {
        let module: Vec<u16> = module_script_text.encode_utf16().collect();

        let url_cstr = ffi::CString::new(url.as_str().as_bytes()).unwrap();

        let _ac = JSAutoRealm::new(*global.get_cx(), *global.reflector().get_jsobject());

        let compile_options =
            unsafe { CompileOptionsWrapper::new(*global.get_cx(), url_cstr.as_ptr(), 1) };

        unsafe {
            rooted!(in(*global.get_cx()) let mut module_script = CompileModule(
                *global.get_cx(),
                compile_options.ptr,
                &mut transform_u16_to_source_text(&module),
            ));

            if module_script.is_null() {
                warn!("fail to compile module script of {}", url);

                rooted!(in(*global.get_cx()) let mut exception = UndefinedValue());
                assert!(JS_GetPendingException(
                    *global.get_cx(),
                    &mut exception.handle_mut()
                ));
                JS_ClearPendingException(*global.get_cx());

                return Err(RethrowError(RootedTraceableBox::from_box(Heap::boxed(
                    exception.get(),
                ))));
            }

            let module_script_data = Box::new(ModuleScript {
                base_url: url.clone(),
            });

            SetModulePrivate(
                module_script.get(),
                &PrivateValue(Box::into_raw(module_script_data) as *const _),
            );

            debug!("module script of {} compile done", url);

            self.resolve_requested_module_specifiers(
                &global,
                module_script.handle().into_handle(),
                url.clone(),
            )
            .map(|_| ModuleObject(Heap::boxed(*module_script)))
        }
    }

    #[allow(unsafe_code)]
    /// https://html.spec.whatwg.org/multipage/#fetch-the-descendants-of-and-link-a-module-script
    /// Step 5-2.
    pub fn instantiate_module_tree(
        &self,
        global: &GlobalScope,
        module_record: HandleObject,
    ) -> Result<(), RethrowError> {
        let _ac = JSAutoRealm::new(*global.get_cx(), *global.reflector().get_jsobject());

        unsafe {
            if !ModuleInstantiate(*global.get_cx(), module_record) {
                warn!("fail to instantiate module");

                rooted!(in(*global.get_cx()) let mut exception = UndefinedValue());
                assert!(JS_GetPendingException(
                    *global.get_cx(),
                    &mut exception.handle_mut()
                ));
                JS_ClearPendingException(*global.get_cx());

                Err(RethrowError(RootedTraceableBox::from_box(Heap::boxed(
                    exception.get(),
                ))))
            } else {
                debug!("module instantiated successfully");

                Ok(())
            }
        }
    }

    #[allow(unsafe_code)]
    pub fn execute_module(
        &self,
        global: &GlobalScope,
        module_record: HandleObject,
    ) -> Result<(), RethrowError> {
        let _ac = JSAutoRealm::new(*global.get_cx(), *global.reflector().get_jsobject());

        unsafe {
            if !ModuleEvaluate(*global.get_cx(), module_record) {
                warn!("fail to evaluate module");

                rooted!(in(*global.get_cx()) let mut exception = UndefinedValue());
                assert!(JS_GetPendingException(
                    *global.get_cx(),
                    &mut exception.handle_mut()
                ));
                JS_ClearPendingException(*global.get_cx());

                Err(RethrowError(RootedTraceableBox::from_box(Heap::boxed(
                    exception.get(),
                ))))
            } else {
                debug!("module evaluated successfully");
                Ok(())
            }
        }
    }

    #[allow(unsafe_code)]
    pub fn report_error(&self, global: &GlobalScope) {
        let module_error = self.rethrow_error.borrow();

        if let Some(exception) = &*module_error {
            unsafe {
                let ar = enter_realm(&*global);
                JS_SetPendingException(
                    *global.get_cx(),
                    exception.handle(),
                    ExceptionStackBehavior::Capture,
                );
                report_pending_exception(*global.get_cx(), true, InRealm::Entered(&ar));
            }
        }
    }

    #[allow(unsafe_code)]
    fn resolve_requested_module_specifiers(
        &self,
        global: &GlobalScope,
        module_object: HandleObject,
        base_url: ServoUrl,
    ) -> Result<IndexSet<ServoUrl>, RethrowError> {
        let _ac = JSAutoRealm::new(*global.get_cx(), *global.reflector().get_jsobject());

        let mut specifier_urls = IndexSet::new();

        unsafe {
            rooted!(in(*global.get_cx()) let requested_modules = GetRequestedModules(*global.get_cx(), module_object));

            let mut length = 0;

            if !JS_GetArrayLength(*global.get_cx(), requested_modules.handle(), &mut length) {
                let module_length_error =
                    gen_type_error(&global, "Wrong length of requested modules".to_owned());

                return Err(module_length_error);
            }

            for index in 0..length {
                rooted!(in(*global.get_cx()) let mut element = UndefinedValue());

                if !JS_GetElement(
                    *global.get_cx(),
                    requested_modules.handle(),
                    index,
                    &mut element.handle_mut(),
                ) {
                    let get_element_error =
                        gen_type_error(&global, "Failed to get requested module".to_owned());

                    return Err(get_element_error);
                }

                rooted!(in(*global.get_cx()) let specifier = GetRequestedModuleSpecifier(
                    *global.get_cx(), element.handle()
                ));

                let url = ModuleTree::resolve_module_specifier(
                    *global.get_cx(),
                    &base_url,
                    specifier.handle().into_handle(),
                );

                if url.is_err() {
                    let specifier_error =
                        gen_type_error(&global, "Wrong module specifier".to_owned());

                    return Err(specifier_error);
                }

                specifier_urls.insert(url.unwrap());
            }
        }

        Ok(specifier_urls)
    }

    /// The following module specifiers are allowed by the spec:
    ///  - a valid absolute URL
    ///  - a valid relative URL that starts with "/", "./" or "../"
    ///
    /// Bareword module specifiers are currently disallowed as these may be given
    /// special meanings in the future.
    /// https://html.spec.whatwg.org/multipage/#resolve-a-module-specifier
    #[allow(unsafe_code)]
    fn resolve_module_specifier(
        cx: *mut JSContext,
        url: &ServoUrl,
        specifier: RawHandle<*mut JSString>,
    ) -> Result<ServoUrl, UrlParseError> {
        let specifier_str = unsafe { jsstring_to_str(cx, *specifier) };

        // Step 1.
        if let Ok(specifier_url) = ServoUrl::parse(&specifier_str) {
            return Ok(specifier_url);
        }

        // Step 2.
        if !specifier_str.starts_with("/") &&
            !specifier_str.starts_with("./") &&
            !specifier_str.starts_with("../")
        {
            return Err(UrlParseError::InvalidDomainCharacter);
        }

        // Step 3.
        return ServoUrl::parse_with_base(Some(url), &specifier_str.clone());
    }

    /// https://html.spec.whatwg.org/multipage/#finding-the-first-parse-error
    fn find_first_parse_error(
        &self,
        global: &GlobalScope,
        discovered_urls: &mut HashSet<ServoUrl>,
    ) -> (Option<NetworkError>, Option<RethrowError>) {
        // 3.
        discovered_urls.insert(self.url.clone());

        // 4.
        let record = self.get_record().borrow();
        if record.is_none() {
            return (
                self.network_error.borrow().clone(),
                self.rethrow_error.borrow().clone(),
            );
        }

        let module_map = global.get_module_map().borrow();
        let mut parse_error: Option<RethrowError> = None;

        // 5-6.
        let descendant_urls = self.descendant_urls.borrow();
        for descendant_module in descendant_urls
            .iter()
            // 7.
            .filter_map(|url| module_map.get(&url.clone()))
        {
            // 8-2.
            if discovered_urls.contains(&descendant_module.url) {
                continue;
            }

            // 8-3.
            let (child_network_error, child_parse_error) =
                descendant_module.find_first_parse_error(&global, discovered_urls);

            // Due to network error's priority higher than parse error,
            // we will return directly when we meet a network error.
            if child_network_error.is_some() {
                return (child_network_error, None);
            }

            // 8-4.
            //
            // In case of having any network error in other descendants,
            // we will store the "first" parse error and keep running this
            // loop to ensure we don't have any network error.
            if child_parse_error.is_some() && parse_error.is_none() {
                parse_error = child_parse_error;
            }
        }

        // Step 9.
        return (None, parse_error);
    }

    #[allow(unsafe_code)]
    /// https://html.spec.whatwg.org/multipage/#fetch-the-descendants-of-a-module-script
    fn fetch_module_descendants(
        &self,
        owner: &ModuleOwner,
        destination: Destination,
        credentials_mode: CredentialsMode,
        parent_identity: ModuleIdentity,
    ) {
        debug!("Start to load dependencies of {}", self.url.clone());

        let global = owner.global();

        self.set_status(ModuleStatus::FetchingDescendants);

        let specifier_urls = {
            let raw_record = self.record.borrow();
            match raw_record.as_ref() {
                // Step 1.
                None => {
                    self.set_status(ModuleStatus::Finished);
                    debug!(
                        "Module {} doesn't have module record but tried to load descendants.",
                        self.url.clone()
                    );
                    return;
                },
                // Step 5.
                Some(raw_record) => self.resolve_requested_module_specifiers(
                    &global,
                    raw_record.handle(),
                    self.url.clone(),
                ),
            }
        };

        match specifier_urls {
            // Step 3.
            Ok(valid_specifier_urls) if valid_specifier_urls.len() == 0 => {
                debug!("Module {} doesn't have any dependencies.", self.url.clone());
                self.advance_finished_and_link(&global);
            },
            Ok(valid_specifier_urls) => {
                self.descendant_urls
                    .borrow_mut()
                    .extend(valid_specifier_urls.clone());

                let mut urls = IndexSet::new();
                let mut visited_urls = self.visited_urls.borrow_mut();

                for parsed_url in valid_specifier_urls {
                    // Step 5-3.
                    if !visited_urls.contains(&parsed_url) {
                        // Step 5-3-1.
                        urls.insert(parsed_url.clone());
                        // Step 5-3-2.
                        visited_urls.insert(parsed_url.clone());

                        self.insert_incomplete_fetch_url(parsed_url.clone());
                    }
                }

                // Step 3.
                if urls.len() == 0 {
                    debug!(
                        "After checking with visited urls, module {} doesn't have dependencies to load.",
                        self.url.clone()
                    );
                    self.advance_finished_and_link(&global);
                    return;
                }

                // Step 8.
                for url in urls {
                    // https://html.spec.whatwg.org/multipage/#internal-module-script-graph-fetching-procedure
                    // Step 1.
                    assert!(visited_urls.get(&url).is_some());

                    // Step 2.
                    fetch_single_module_script(
                        owner.clone(),
                        url.clone(),
                        visited_urls.clone(),
                        destination.clone(),
                        Referrer::Client,
                        ParserMetadata::NotParserInserted,
                        "".to_owned(), // integrity
                        credentials_mode.clone(),
                        Some(parent_identity.clone()),
                        false,
                    );
                }
            },
            Err(error) => {
                self.set_rethrow_error(error);
                self.advance_finished_and_link(&global);
            },
        }
    }

    /// https://html.spec.whatwg.org/multipage/#fetch-the-descendants-of-and-link-a-module-script
    /// step 4-7.
    fn advance_finished_and_link(&self, global: &GlobalScope) {
        {
            let descendant_urls = self.descendant_urls.borrow();

            // Check if there's any dependencies under fetching.
            //
            // We can't only check `incomplete fetches` here because...
            //
            // For example, module `A` has descendants `B`, `C`
            // while `A` has added them to incomplete fetches, it's possible
            // `B` has finished but `C` is not yet fired its fetch; in this case,
            // `incomplete fetches` will be `zero` but the module is actually not ready
            // to finish. Thus, we need to check dependencies directly instead of
            // incomplete fetches here.
            if !is_all_dependencies_ready(&descendant_urls, &global) {
                // When we found the `incomplete fetches` is bigger than zero,
                // we will need to check if there's any circular dependency.
                //
                // If there's no circular dependencies but there are incomplete fetches,
                // it means it needs to wait for finish.
                //
                // Or, if there are circular dependencies, then we need to confirm
                // no circular dependencies are fetching.
                //
                // if there's any circular dependencies and they all proceeds to status
                // higher than `FetchingDescendants`, then it means we can proceed to finish.
                let circular_deps = self.find_circular_dependencies(&global);

                if circular_deps.len() == 0 || !is_all_dependencies_ready(&circular_deps, &global) {
                    return;
                }
            }
        }

        self.set_status(ModuleStatus::Finished);

        debug!("Going to advance and finish for: {}", self.url.clone());

        {
            // Notify parents of this module to finish
            //
            // Before notifying, if the parent module has already had zero incomplete
            // fetches, then it means we don't need to notify it.
            let parent_identities = self.parent_identities.borrow();
            for parent_identity in parent_identities.iter() {
                let parent_tree = parent_identity.get_module_tree(&global);

                let incomplete_count_before_remove = {
                    let incomplete_urls = parent_tree.get_incomplete_fetch_urls().borrow();
                    incomplete_urls.len()
                };

                if incomplete_count_before_remove > 0 {
                    parent_tree.remove_incomplete_fetch_url(self.url.clone());
                    parent_tree.advance_finished_and_link(&global);
                }
            }
        }

        let mut discovered_urls: HashSet<ServoUrl> = HashSet::new();
        let (network_error, rethrow_error) =
            self.find_first_parse_error(&global, &mut discovered_urls);

        match (network_error, rethrow_error) {
            (Some(network_error), _) => {
                self.set_network_error(network_error);
            },
            (None, None) => {
                let module_record = self.get_record().borrow();
                if let Some(record) = &*module_record {
                    let instantiated = self.instantiate_module_tree(&global, record.handle());

                    if let Err(exception) = instantiated {
                        self.set_rethrow_error(exception);
                    }
                }
            },
            (None, Some(error)) => {
                self.set_rethrow_error(error);
            },
        }

        let promise = self.promise.borrow();
        if let Some(promise) = promise.as_ref() {
            promise.resolve_native(&());
        }
    }
}

// Iterate the given dependency urls to see if it and its descendants are fetching or not.
// When a module status is `FetchingDescendants`, it's possible that the module is a circular
// module so we will also check its descendants.
fn is_all_dependencies_ready(dependencies: &IndexSet<ServoUrl>, global: &GlobalScope) -> bool {
    dependencies.iter().all(|dep| {
        let module_map = global.get_module_map().borrow();
        match module_map.get(&dep) {
            Some(module) => {
                let module_descendants = module.get_descendant_urls().borrow();

                module.get_status() >= ModuleStatus::FetchingDescendants &&
                    module_descendants.iter().all(|descendant_url| {
                        match module_map.get(&descendant_url) {
                            Some(m) => m.get_status() >= ModuleStatus::FetchingDescendants,
                            None => false,
                        }
                    })
            },
            None => false,
        }
    })
}

#[derive(JSTraceable, MallocSizeOf)]
struct ModuleHandler {
    #[ignore_malloc_size_of = "Measuring trait objects is hard"]
    task: DomRefCell<Option<Box<dyn TaskBox>>>,
}

impl ModuleHandler {
    pub fn new(task: Box<dyn TaskBox>) -> Box<dyn Callback> {
        Box::new(Self {
            task: DomRefCell::new(Some(task)),
        })
    }
}

impl Callback for ModuleHandler {
    fn callback(&self, _cx: SafeJSContext, _v: HandleValue, _realm: InRealm) {
        let task = self.task.borrow_mut().take().unwrap();
        task.run_box();
    }
}

/// The owner of the module
/// It can be `worker` or `script` element
#[derive(Clone)]
pub enum ModuleOwner {
    #[allow(dead_code)]
    Worker(TrustedWorkerAddress),
    Window(Trusted<HTMLScriptElement>),
}

impl ModuleOwner {
    pub fn global(&self) -> DomRoot<GlobalScope> {
        match &self {
            ModuleOwner::Worker(worker) => (*worker.root().clone()).global(),
            ModuleOwner::Window(script) => (*script.root()).global(),
        }
    }

    pub fn notify_owner_to_finish(&self, module_identity: ModuleIdentity) {
        match &self {
            ModuleOwner::Worker(_) => unimplemented!(),
            ModuleOwner::Window(script) => {
                let global = self.global();

                let document = document_from_node(&*script.root());

                let load = {
                    let module_tree = module_identity.get_module_tree(&global);

                    let network_error = module_tree.get_network_error().borrow();
                    match network_error.as_ref() {
                        Some(network_error) => Err(network_error.clone()),
                        None => match module_identity {
                            ModuleIdentity::ModuleUrl(script_src) => Ok(ScriptOrigin::external(
                                module_tree.get_text().borrow().clone(),
                                script_src.clone(),
                                ScriptType::Module,
                            )),
                            ModuleIdentity::ScriptId(_) => Ok(ScriptOrigin::internal(
                                module_tree.get_text().borrow().clone(),
                                document.base_url().clone(),
                                ScriptType::Module,
                            )),
                        },
                    }
                };

                let r#async = script
                    .root()
                    .upcast::<Element>()
                    .has_attribute(&local_name!("async"));

                if !r#async && (&*script.root()).get_parser_inserted() {
                    document.deferred_script_loaded(&*script.root(), load);
                } else if !r#async && !(&*script.root()).get_non_blocking() {
                    document.asap_in_order_script_loaded(&*script.root(), load);
                } else {
                    document.asap_script_loaded(&*script.root(), load);
                };
            },
        }
    }
}

/// The context required for asynchronously loading an external module script source.
struct ModuleContext {
    /// The owner of the module that initiated the request.
    owner: ModuleOwner,
    /// The response body received to date.
    data: Vec<u8>,
    /// The response metadata received to date.
    metadata: Option<Metadata>,
    /// The initial URL requested.
    url: ServoUrl,
    /// Destination of current module context
    destination: Destination,
    /// Credentials Mode of current module context
    credentials_mode: CredentialsMode,
    /// Indicates whether the request failed, and why
    status: Result<(), NetworkError>,
    /// Timing object for this resource
    resource_timing: ResourceFetchTiming,
}

impl FetchResponseListener for ModuleContext {
    fn process_request_body(&mut self) {} // TODO(cybai): Perhaps add custom steps to perform fetch here?

    fn process_request_eof(&mut self) {} // TODO(cybai): Perhaps add custom steps to perform fetch here?

    fn process_response(&mut self, metadata: Result<FetchMetadata, NetworkError>) {
        self.metadata = metadata.ok().map(|meta| match meta {
            FetchMetadata::Unfiltered(m) => m,
            FetchMetadata::Filtered { unsafe_, .. } => unsafe_,
        });

        let status_code = self
            .metadata
            .as_ref()
            .and_then(|m| match m.status {
                Some((c, _)) => Some(c),
                _ => None,
            })
            .unwrap_or(0);

        self.status = match status_code {
            0 => Err(NetworkError::Internal(
                "No http status code received".to_owned(),
            )),
            200..=299 => Ok(()), // HTTP ok status codes
            _ => Err(NetworkError::Internal(format!(
                "HTTP error code {}",
                status_code
            ))),
        };
    }

    fn process_response_chunk(&mut self, mut chunk: Vec<u8>) {
        if self.status.is_ok() {
            self.data.append(&mut chunk);
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#fetch-a-single-module-script>
    /// Step 9-12
    #[allow(unsafe_code)]
    fn process_response_eof(&mut self, response: Result<ResourceFetchTiming, NetworkError>) {
        let global = self.owner.global();

        if let Some(window) = global.downcast::<Window>() {
            window
                .Document()
                .finish_load(LoadType::Script(self.url.clone()));
        }

        // Step 9-1 & 9-2.
        let load = response.and(self.status.clone()).and_then(|_| {
            // Step 9-3.
            let meta = self.metadata.take().unwrap();

            if let Some(content_type) = meta.content_type.map(Serde::into_inner) {
                if let Ok(content_type) = Mime::from_str(&content_type.to_string()) {
                    let essence_mime = content_type.essence_str();

                    if !SCRIPT_JS_MIMES.contains(&essence_mime) {
                        return Err(NetworkError::Internal(format!(
                            "Invalid MIME type: {}",
                            essence_mime
                        )));
                    }
                } else {
                    return Err(NetworkError::Internal(format!(
                        "Failed to parse MIME type: {}",
                        content_type.to_string()
                    )));
                }
            } else {
                return Err(NetworkError::Internal("No MIME type".into()));
            }

            // Step 10.
            let (source_text, _, _) = UTF_8.decode(&self.data);
            Ok(ScriptOrigin::external(
                DOMString::from(source_text),
                meta.final_url,
                ScriptType::Module,
            ))
        });

        let module_tree = {
            let module_map = global.get_module_map().borrow();
            module_map.get(&self.url.clone()).unwrap().clone()
        };

        module_tree.remove_incomplete_fetch_url(self.url.clone());

        // Step 12.
        match load {
            Err(err) => {
                error!("Failed to fetch {} with error {:?}", self.url.clone(), err);
                module_tree.set_network_error(err);
                module_tree.advance_finished_and_link(&global);
            },
            Ok(ref resp_mod_script) => {
                module_tree.set_text(resp_mod_script.text());

                let compiled_module = module_tree.compile_module_script(
                    &global,
                    resp_mod_script.text(),
                    self.url.clone(),
                );

                match compiled_module {
                    Err(exception) => {
                        module_tree.set_rethrow_error(exception);
                        module_tree.advance_finished_and_link(&global);
                    },
                    Ok(record) => {
                        module_tree.set_record(record);

                        module_tree.fetch_module_descendants(
                            &self.owner,
                            self.destination.clone(),
                            self.credentials_mode.clone(),
                            ModuleIdentity::ModuleUrl(self.url.clone()),
                        );
                    },
                }
            },
        }
    }

    fn resource_timing_mut(&mut self) -> &mut ResourceFetchTiming {
        &mut self.resource_timing
    }

    fn resource_timing(&self) -> &ResourceFetchTiming {
        &self.resource_timing
    }

    fn submit_resource_timing(&mut self) {
        network_listener::submit_timing(self)
    }
}

impl ResourceTimingListener for ModuleContext {
    fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
        let initiator_type = InitiatorType::LocalName("module".to_string());
        (initiator_type, self.url.clone())
    }

    fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
        self.owner.global()
    }
}

impl PreInvoke for ModuleContext {}

#[allow(unsafe_code, non_snake_case)]
/// A function to register module hooks (e.g. listening on resolving modules,
/// getting module metadata, getting script private reference and resolving dynamic import)
pub unsafe fn EnsureModuleHooksInitialized(rt: *mut JSRuntime) {
    if GetModuleResolveHook(rt).is_some() {
        return;
    }

    SetModuleResolveHook(rt, Some(HostResolveImportedModule));
    SetModuleMetadataHook(rt, Some(HostPopulateImportMeta));
    SetScriptPrivateReferenceHooks(rt, None, None);

    SetModuleDynamicImportHook(rt, None);
}

#[allow(unsafe_code, non_snake_case)]
/// https://tc39.github.io/ecma262/#sec-hostresolveimportedmodule
/// https://html.spec.whatwg.org/multipage/#hostresolveimportedmodule(referencingscriptormodule%2C-specifier)
unsafe extern "C" fn HostResolveImportedModule(
    cx: *mut JSContext,
    reference_private: RawHandleValue,
    specifier: RawHandle<*mut JSString>,
) -> *mut JSObject {
    let in_realm_proof = AlreadyInRealm::assert_for_cx(SafeJSContext::from_ptr(cx));
    let global_scope = GlobalScope::from_context(cx, InRealm::Already(&in_realm_proof));

    // Step 2.
    let mut base_url = global_scope.api_base_url();

    // Step 3.
    let module_data = (reference_private.to_private() as *const ModuleScript).as_ref();
    if let Some(data) = module_data {
        base_url = data.base_url.clone();
    }

    // Step 5.
    let url = ModuleTree::resolve_module_specifier(*global_scope.get_cx(), &base_url, specifier);

    // Step 6.
    assert!(url.is_ok());

    let parsed_url = url.unwrap();

    // Step 4 & 7.
    let module_map = global_scope.get_module_map().borrow();

    let module_tree = module_map.get(&parsed_url);

    // Step 9.
    assert!(module_tree.is_some());

    let fetched_module_object = module_tree.unwrap().get_record().borrow();

    // Step 8.
    assert!(fetched_module_object.is_some());

    // Step 10.
    if let Some(record) = &*fetched_module_object {
        return record.handle().get();
    }

    unreachable!()
}

#[allow(unsafe_code, non_snake_case)]
/// https://tc39.es/ecma262/#sec-hostgetimportmetaproperties
/// https://html.spec.whatwg.org/multipage/#hostgetimportmetaproperties
unsafe extern "C" fn HostPopulateImportMeta(
    cx: *mut JSContext,
    reference_private: RawHandleValue,
    meta_object: RawHandle<*mut JSObject>,
) -> bool {
    let in_realm_proof = AlreadyInRealm::assert_for_cx(SafeJSContext::from_ptr(cx));
    let global_scope = GlobalScope::from_context(cx, InRealm::Already(&in_realm_proof));

    // Step 2.
    let base_url = match (reference_private.to_private() as *const ModuleScript).as_ref() {
        Some(module_data) => module_data.base_url.clone(),
        None => global_scope.api_base_url(),
    };

    rooted!(in(cx) let url_string = JS_NewStringCopyN(
        cx,
        base_url.as_str().as_ptr() as *const i8,
        base_url.as_str().len()
    ));

    // Step 3.
    JS_DefineProperty4(
        cx,
        meta_object,
        "url\0".as_ptr() as *const i8,
        url_string.handle().into_handle(),
        JSPROP_ENUMERATE.into(),
    )
}

/// https://html.spec.whatwg.org/multipage/#fetch-a-module-script-tree
pub fn fetch_external_module_script(
    owner: ModuleOwner,
    url: ServoUrl,
    destination: Destination,
    integrity_metadata: String,
    credentials_mode: CredentialsMode,
) {
    let mut visited_urls = HashSet::new();
    visited_urls.insert(url.clone());

    // Step 1.
    fetch_single_module_script(
        owner,
        url,
        visited_urls,
        destination,
        Referrer::Client,
        ParserMetadata::NotParserInserted,
        integrity_metadata,
        credentials_mode,
        None,
        true,
    );
}

/// https://html.spec.whatwg.org/multipage/#fetch-a-single-module-script
pub fn fetch_single_module_script(
    owner: ModuleOwner,
    url: ServoUrl,
    visited_urls: HashSet<ServoUrl>,
    destination: Destination,
    referrer: Referrer,
    parser_metadata: ParserMetadata,
    integrity_metadata: String,
    credentials_mode: CredentialsMode,
    parent_identity: Option<ModuleIdentity>,
    top_level_module_fetch: bool,
) {
    {
        // Step 1.
        let global = owner.global();
        let module_map = global.get_module_map().borrow();

        debug!("Start to fetch {}", url);

        if let Some(module_tree) = module_map.get(&url.clone()) {
            let status = module_tree.get_status();

            debug!("Meet a fetched url {} and its status is {:?}", url, status);

            if top_level_module_fetch {
                module_tree.append_handler(owner.clone(), ModuleIdentity::ModuleUrl(url.clone()));
            }

            if let Some(parent_identity) = parent_identity {
                module_tree.insert_parent_identity(parent_identity);
            }

            match status {
                ModuleStatus::Initial => unreachable!(
                    "We have the module in module map so its status should not be `initial`"
                ),
                // Step 2.
                ModuleStatus::Fetching => {},
                // Step 3.
                ModuleStatus::FetchingDescendants | ModuleStatus::Finished => {
                    module_tree.advance_finished_and_link(&global);
                },
            }

            return;
        }
    }

    let global = owner.global();
    let is_external = true;
    let module_tree = ModuleTree::new(url.clone(), is_external, visited_urls);
    module_tree.set_status(ModuleStatus::Fetching);

    if top_level_module_fetch {
        module_tree.append_handler(owner.clone(), ModuleIdentity::ModuleUrl(url.clone()));
    }

    if let Some(parent_identity) = parent_identity {
        module_tree.insert_parent_identity(parent_identity);
    }

    module_tree.insert_incomplete_fetch_url(url.clone());

    // Step 4.
    global.set_module_map(url.clone(), module_tree);

    // Step 5-6.
    let mode = match destination.clone() {
        Destination::Worker | Destination::SharedWorker if top_level_module_fetch => {
            RequestMode::SameOrigin
        },
        _ => RequestMode::CorsMode,
    };

    let document: Option<DomRoot<Document>> = match &owner {
        ModuleOwner::Worker(_) => None,
        ModuleOwner::Window(script) => Some(document_from_node(&*script.root())),
    };

    // Step 7-8.
    let request = RequestBuilder::new(url.clone())
        .destination(destination.clone())
        .origin(global.origin().immutable().clone())
        .referrer(Some(referrer))
        .parser_metadata(parser_metadata)
        .integrity_metadata(integrity_metadata.clone())
        .credentials_mode(credentials_mode)
        .mode(mode);

    let context = Arc::new(Mutex::new(ModuleContext {
        owner,
        data: vec![],
        metadata: None,
        url: url.clone(),
        destination: destination.clone(),
        credentials_mode: credentials_mode.clone(),
        status: Ok(()),
        resource_timing: ResourceFetchTiming::new(ResourceTimingType::Resource),
    }));

    let (action_sender, action_receiver) = ipc::channel().unwrap();
    let task_source = global.networking_task_source();
    let canceller = global.task_canceller(TaskSourceName::Networking);

    let listener = NetworkListener {
        context,
        task_source,
        canceller: Some(canceller),
    };

    ROUTER.add_route(
        action_receiver.to_opaque(),
        Box::new(move |message| {
            listener.notify_fetch(message.to().unwrap());
        }),
    );

    if let Some(doc) = document {
        doc.fetch_async(LoadType::Script(url), request, action_sender);
    }
}

#[allow(unsafe_code)]
/// https://html.spec.whatwg.org/multipage/#fetch-an-inline-module-script-graph
pub fn fetch_inline_module_script(
    owner: ModuleOwner,
    module_script_text: DOMString,
    url: ServoUrl,
    script_id: ScriptId,
    credentials_mode: CredentialsMode,
) {
    let global = owner.global();
    let is_external = false;
    let module_tree = ModuleTree::new(url.clone(), is_external, HashSet::new());

    let compiled_module =
        module_tree.compile_module_script(&global, module_script_text, url.clone());

    match compiled_module {
        Ok(record) => {
            module_tree.append_handler(owner.clone(), ModuleIdentity::ScriptId(script_id.clone()));
            module_tree.set_record(record);

            // We need to set `module_tree` into inline module map in case
            // of that the module descendants finished right after the
            // fetch module descendants step.
            global.set_inline_module_map(script_id, module_tree);

            // Due to needed to set `module_tree` to inline module_map first,
            // we will need to retrieve it again so that we can do the fetch
            // module descendants step.
            let inline_module_map = global.get_inline_module_map().borrow();
            let module_tree = inline_module_map.get(&script_id).unwrap().clone();

            module_tree.fetch_module_descendants(
                &owner,
                Destination::Script,
                credentials_mode,
                ModuleIdentity::ScriptId(script_id),
            );
        },
        Err(exception) => {
            module_tree.set_rethrow_error(exception);
            module_tree.set_status(ModuleStatus::Finished);
            global.set_inline_module_map(script_id.clone(), module_tree);
            owner.notify_owner_to_finish(ModuleIdentity::ScriptId(script_id));
        },
    }
}