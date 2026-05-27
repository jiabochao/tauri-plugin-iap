use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use tauri::Emitter;
use tauri::Manager;
use tauri::{AppHandle, Runtime, plugin::PluginApi};
use windows::core::{HSTRING, Interface};
use windows::{
    Foundation::DateTime,
    Services::Store::{
        StoreConsumableStatus, StoreContext, StoreLicense, StoreProduct, StorePurchaseStatus,
    },
    Win32::UI::Shell::IInitializeWithWindow,
};
use windows_collections::IIterable;

use crate::error::{ErrorResponse, PluginInvokeError};
use crate::models::{
    GetProductsResponse, PricingPhase, Product, ProductStatus, Purchase, PurchaseRequest,
    PurchaseStateValue, RestorePurchasesResponse, SubscriptionOffer,
};

/// Opaque token used only by the Windows implementation.
///
/// Public APIs accept and return the app's Partner Center product id
/// (`StoreProduct.InAppOfferToken`). Windows Store ids and SKU ids stay inside
/// this module. Consumable fulfillment still needs the Microsoft product
/// StoreId, so the opaque token stores it privately.
#[derive(Serialize, Deserialize)]
struct WindowsPurchaseTokenV1 {
    v: u8,
    store_id: String,
    purchase_time: i64,
    nonce: u32,
}

impl WindowsPurchaseTokenV1 {
    fn encode(&self) -> crate::Result<String> {
        let bytes = serde_json::to_vec(self).map_err(|e| {
            crate::Error::PluginInvoke(PluginInvokeError::InvokeRejected(ErrorResponse {
                code: Some("internalError".to_string()),
                message: Some(format!("Failed to encode purchase token: {e}")),
                data: (),
            }))
        })?;
        Ok(URL_SAFE_NO_PAD.encode(&bytes))
    }

    fn decode(s: &str) -> crate::Result<Self> {
        let invalid = || {
            crate::Error::PluginInvoke(PluginInvokeError::InvokeRejected(ErrorResponse {
                code: Some("invalidPurchaseToken".to_string()),
                message: Some("Invalid Windows purchase token".to_string()),
                data: (),
            }))
        };

        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid())?;
        let env: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if env.v != 1 || env.store_id.trim().is_empty() {
            return Err(invalid());
        }
        Ok(env)
    }
}

#[allow(clippy::unnecessary_wraps)]
pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: &PluginApi<R, C>,
) -> crate::Result<Iap<R>> {
    Ok(Iap {
        app_handle: app.clone(),
        store_context: Arc::new(RwLock::new(None)),
    })
}

/// Access to the IAP APIs.
pub struct Iap<R: Runtime> {
    app_handle: AppHandle<R>,
    store_context: Arc<RwLock<Option<StoreContext>>>,
}

impl<R: Runtime> Iap<R> {
    fn reject(code: &str, message: impl Into<String>) -> crate::Error {
        crate::Error::PluginInvoke(PluginInvokeError::InvokeRejected(ErrorResponse {
            code: Some(code.to_string()),
            message: Some(message.into()),
            data: (),
        }))
    }

    fn now_millis() -> crate::Result<i64> {
        // Generate purchase details using the current Unix time in milliseconds.
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| {
                Self::reject(
                    "systemTimeError",
                    format!("Failed to get system time: {e:?}"),
                )
            })?
            .as_millis();

        i64::try_from(ms).map_err(|e| {
            Self::reject(
                "systemTimeError",
                format!("System time out of i64 range: {e}"),
            )
        })
    }

    /// Get or create the `StoreContext` instance.
    fn get_store_context(&self) -> crate::Result<StoreContext> {
        let mut context_guard = self.store_context.write().map_err(|e| {
            Self::reject(
                "internalError",
                format!("Failed to acquire write lock: {e:?}"),
            )
        })?;

        if context_guard.is_none() {
            // Get the default store context for the current user.
            let context = StoreContext::GetDefault()?;

            let window = self
                .app_handle
                .get_webview_window("main")
                .ok_or_else(|| Self::reject("windowError", "Failed to get main window"))?;
            let hwnd = window.hwnd().map_err(|e| {
                Self::reject("windowError", format!("Failed to get window handle: {e:?}"))
            })?;

            // Cast the WinRT object to IInitializeWithWindow and initialize it with your HWND.
            let init = context.cast::<IInitializeWithWindow>()?;
            unsafe {
                init.Initialize(hwnd)?;
            }

            *context_guard = Some(context);
        }

        Ok(context_guard
            .as_ref()
            .ok_or_else(|| Self::reject("storeNotInitialized", "Store context not initialized"))?
            .clone())
    }

    /// Convert Windows `DateTime` to Unix timestamp in milliseconds.
    const fn datetime_to_unix_millis(datetime: DateTime) -> i64 {
        // Windows DateTime is in 100-nanosecond intervals since January 1, 1601.
        // Convert to Unix timestamp (milliseconds since January 1, 1970).
        const WINDOWS_TICK: i64 = 10_000_000;
        const SEC_TO_UNIX_EPOCH: i64 = 11_644_473_600;

        let seconds_since_1601 = datetime.UniversalTime / WINDOWS_TICK;
        let unix_seconds = seconds_since_1601 - SEC_TO_UNIX_EPOCH;
        unix_seconds * 1000
    }

    /// Emit an event to the frontend (equivalent to iOS/Android `trigger` method).
    fn trigger<S: serde::Serialize + Clone>(&self, event: &str, payload: S) {
        let _ = self.app_handle.emit(event, payload);
    }

    /// Determine Windows product kinds based on the cross-platform product type.
    fn product_kinds(product_type: &str) -> Vec<HSTRING> {
        match product_type {
            "inapp" => vec![
                HSTRING::from("Consumable"),
                HSTRING::from("UnmanagedConsumable"),
            ],
            "subs" => vec![HSTRING::from("Subscription"), HSTRING::from("Durable")],
            _ => vec![
                HSTRING::from("Consumable"),
                HSTRING::from("UnmanagedConsumable"),
                HSTRING::from("Durable"),
                HSTRING::from("Subscription"),
            ],
        }
    }

    /// Return the developer-defined product id.
    ///
    /// In Microsoft Store APIs this is exposed as InAppOfferToken. StoreId and
    /// SkuStoreId are Microsoft-generated identifiers and should remain internal.
    fn app_product_id(store_product: &StoreProduct) -> crate::Result<String> {
        let product_id = store_product.InAppOfferToken()?.to_string();
        if product_id.trim().is_empty() {
            return Err(Self::reject(
                "missingProductId",
                "Windows Store product is missing InAppOfferToken",
            ));
        }
        Ok(product_id)
    }

    /// Convert a SKU StoreId like `9NXXXX/000N` back to the product StoreId.
    fn store_id_from_sku_store_id(sku_store_id: &str) -> String {
        sku_store_id
            .split('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(sku_store_id)
            .to_string()
    }

    /// Build the opaque Windows purchase token used by `consume_purchase()`.
    fn purchase_token_for_store_id(store_id: String, purchase_time: i64) -> crate::Result<String> {
        // Sub-millisecond entropy so two purchases in the same `purchase_time` ms still differ.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());

        WindowsPurchaseTokenV1 {
            v: 1,
            store_id,
            purchase_time,
            nonce,
        }
        .encode()
    }

    fn query_associated_products(&self, product_type: &str) -> crate::Result<Vec<StoreProduct>> {
        let context = self.get_store_context()?;
        // Determine product kinds based on type.
        let product_kinds: IIterable<HSTRING> = Self::product_kinds(product_type).into();

        // Query all add-ons associated with this app. We cannot use
        // GetStoreProductsAsync with developer product ids because Windows
        // expects Microsoft-generated StoreIds there.
        let query_result = context
            .GetAssociatedStoreProductsAsync(&product_kinds)
            .and_then(|async_op| async_op.get())?;

        // Check for any errors returned by the Store query.
        let extended_error = query_result.ExtendedError()?;
        if extended_error.is_err() {
            return Err(Self::reject(
                "storeQueryFailed",
                format!(
                    "Store query failed with error: {:?}",
                    extended_error.message()
                ),
            ));
        }

        let products_map = query_result.Products()?;
        let mut products = Vec::new();
        let iterator = products_map.First()?;

        // Iterate through the products and keep the raw StoreProduct values so
        // callers can convert or purchase them after matching by InAppOfferToken.
        while iterator.HasCurrent()? {
            products.push(iterator.Current()?.Value()?);
            iterator.MoveNext()?;
        }

        Ok(products)
    }

    fn find_product_by_product_id(
        &self,
        requested_product_id: &str,
        product_type: &str,
    ) -> crate::Result<StoreProduct> {
        for product in self.query_associated_products(product_type)? {
            // Match only the developer-defined product id exposed by InAppOfferToken.
            if Self::app_product_id(&product)? == requested_product_id {
                return Ok(product);
            }
        }

        Err(Self::reject(
            "productNotFound",
            format!("Product not found: {requested_product_id}"),
        ))
    }

    #[allow(clippy::unused_async)]
    pub async fn get_products(
        &self,
        product_ids: Vec<String>,
        product_type: String,
    ) -> crate::Result<GetProductsResponse> {
        // Fetch associated products once, then map requested developer product ids
        // to the matching Windows StoreProduct instances.
        let store_products = self.query_associated_products(&product_type)?;
        let mut products = Vec::new();

        for requested_id in product_ids {
            let Some(store_product) = store_products
                .iter()
                .find(|product| Self::app_product_id(product).is_ok_and(|id| id == requested_id))
            else {
                continue;
            };

            products.push(Self::convert_store_product_to_product(
                store_product,
                &product_type,
            )?);
        }

        Ok(GetProductsResponse { products })
    }

    fn convert_store_product_to_product(
        store_product: &StoreProduct,
        product_type: &str,
    ) -> crate::Result<Product> {
        let product_id = Self::app_product_id(store_product)?;
        let title = store_product.Title()?.to_string();
        let description = store_product.Description()?.to_string();
        let price = store_product.Price()?;
        let formatted_price = price.FormattedPrice()?.to_string();
        let currency_code = price.CurrencyCode()?.to_string();
        // Get the raw price value.
        let formatted_base_price = price.FormattedBasePrice()?.to_string();

        // Parse price to get numeric value (remove currency symbols).
        let price_value = formatted_base_price
            .chars()
            .filter(|c| c.is_numeric() || *c == '.')
            .collect::<String>()
            .parse::<f64>()
            .unwrap_or(0.0);

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let price_amount_micros = (price_value * 1_000_000.0) as i64;

        // Handle subscription offers if this is a subscription product.
        let subscription_offer_details = if product_type == "subs" {
            // Get SKUs for subscription details.
            let skus = store_product.Skus()?;
            let mut offers = Vec::new();

            for i in 0..skus.Size()? {
                let sku = skus.GetAt(i)?;
                // Check if this SKU has subscription info.
                let Ok(info) = sku.SubscriptionInfo() else {
                    continue;
                };

                let billing_period = info.BillingPeriod()?;
                let billing_period_unit = info.BillingPeriodUnit()?;
                let sku_price = sku.Price()?;
                let billing_period_str = format!(
                    "P{}{}",
                    billing_period,
                    match billing_period_unit.0 {
                        0 => "D",
                        1 => "W",
                        3 => "Y",
                        _ => "M",
                    }
                );

                offers.push(SubscriptionOffer {
                    // Keep Windows SKU ids out of the public API. Product id is
                    // sufficient because this implementation does not expose
                    // per-SKU selection to callers.
                    offer_token: product_id.clone(),
                    base_plan_id: product_id.clone(),
                    offer_id: None,
                    pricing_phases: vec![PricingPhase {
                        formatted_price: sku_price.FormattedPrice()?.to_string(),
                        price_currency_code: currency_code.clone(),
                        price_amount_micros,
                        billing_period: billing_period_str,
                        billing_cycle_count: 0, // Windows doesn't provide this directly.
                        recurrence_mode: 1,     // Infinite recurring.
                    }],
                });
            }

            (!offers.is_empty()).then_some(offers)
        } else {
            None
        };

        Ok(Product {
            product_id,
            title,
            description,
            product_type: product_type.to_string(),
            formatted_price: Some(formatted_price),
            price_currency_code: Some(currency_code),
            price_amount_micros: Some(price_amount_micros),
            subscription_offer_details,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub async fn purchase(&self, payload: PurchaseRequest) -> crate::Result<Purchase> {
        let context = self.get_store_context()?;
        // Get the product first to ensure it exists, resolving the developer
        // product id to the Windows StoreProduct internally.
        let store_product =
            self.find_product_by_product_id(&payload.product_id, &payload.product_type)?;
        let product =
            Self::convert_store_product_to_product(&store_product, &payload.product_type)?;
        let store_id = store_product.StoreId()?.to_string();

        // Simple purchase using the Microsoft StoreId. Callers never need to pass this id.
        let purchase_result = context
            .RequestPurchaseAsync(&HSTRING::from(store_id.as_str()))
            .and_then(|async_op| async_op.get())?;

        // Check purchase status.
        let status = purchase_result.Status()?;
        let purchase_state = match status {
            StorePurchaseStatus::Succeeded | StorePurchaseStatus::AlreadyPurchased => {
                PurchaseStateValue::Purchased
            }
            StorePurchaseStatus::NotPurchased => {
                return Err(Self::reject(
                    "purchaseNotCompleted",
                    "Purchase was not completed",
                ));
            }
            StorePurchaseStatus::NetworkError => {
                return Err(Self::reject(
                    "networkError",
                    "Network error during purchase",
                ));
            }
            StorePurchaseStatus::ServerError => {
                return Err(Self::reject("serverError", "Server error during purchase"));
            }
            _ => return Err(Self::reject("purchaseFailed", "Purchase failed")),
        };

        // Get extended error info if available.
        let error_message = purchase_result
            .ExtendedError()
            .ok()
            .map_or_else(String::new, windows::core::HRESULT::message);

        // Generate purchase details.
        let purchase_time = Self::now_millis()?;
        let purchase_token = Self::purchase_token_for_store_id(store_id, purchase_time)?;

        let purchase = Purchase {
            order_id: Some(purchase_token.clone()),
            package_name: product.title.clone(),
            product_id: product.product_id.clone(),
            purchase_time,
            purchase_token,
            purchase_state,
            is_auto_renewing: product.product_type == "subs",
            is_acknowledged: true, // Windows Store handles acknowledgment.
            original_json: format!(
                r#"{{"status":{},"message":"{}","productId":"{}"}}"#,
                status.0, error_message, product.product_id
            ),
            signature: String::new(), // Windows doesn't provide signatures like Android.
            original_id: None, // Windows doesn't have original transaction IDs like iOS/macOS.
            jws_representation: None, // Windows doesn't have JWS like iOS/macOS.
        };

        // Emit event for purchase state change.
        self.trigger("purchaseUpdated", purchase.clone());
        Ok(purchase)
    }

    #[allow(clippy::unused_async)]
    pub async fn restore_purchases(
        &self,
        product_type: String,
    ) -> crate::Result<RestorePurchasesResponse> {
        // Get app license info.
        let app_license = self
            .get_store_context()?
            .GetAppLicenseAsync()
            .and_then(|async_op| async_op.get())?;
        // Get add-on licenses (in-app purchases).
        let addon_licenses = app_license.AddOnLicenses()?;
        let mut purchases = Vec::new();

        // Enumerate licenses instead of looking them up by key. AddOnLicenses is
        // keyed by SKU StoreId, while the public API accepts developer product ids.
        let iterator = addon_licenses.First()?;
        while iterator.HasCurrent()? {
            let license = iterator.Current()?.Value()?;
            let purchase = self.convert_license_to_purchase(&license, &product_type)?;

            if purchase.purchase_state == PurchaseStateValue::Purchased {
                purchases.push(purchase);
            }

            iterator.MoveNext()?;
        }

        Ok(RestorePurchasesResponse { purchases })
    }

    fn convert_license_to_purchase(
        &self,
        license: &StoreLicense,
        product_type: &str,
    ) -> crate::Result<Purchase> {
        let product_id = license.InAppOfferToken()?.to_string();
        let sku_store_id = license.SkuStoreId()?.to_string();
        // Consumable fulfillment needs the product StoreId, which is the prefix
        // of the SKU StoreId returned by the license.
        let store_id = Self::store_id_from_sku_store_id(&sku_store_id);
        let is_active = license.IsActive()?;
        let expiration_millis = Self::datetime_to_unix_millis(license.ExpirationDate()?);

        // Estimate purchase time (30 days before expiration for monthly subs).
        let purchase_time = if product_type == "subs" && expiration_millis > 0 {
            expiration_millis - (30 * 24 * 60 * 60 * 1000)
        } else {
            Self::now_millis()?
        };
        let purchase_token = Self::purchase_token_for_store_id(store_id, purchase_time)?;

        let purchase_state = if is_active {
            PurchaseStateValue::Purchased
        } else {
            PurchaseStateValue::Canceled
        };

        Ok(Purchase {
            order_id: Some(purchase_token.clone()),
            package_name: self.app_handle.package_info().name.clone(),
            product_id,
            purchase_time,
            purchase_token,
            purchase_state,
            is_auto_renewing: product_type == "subs" && is_active,
            is_acknowledged: true,
            original_json: format!(
                r#"{{"isActive":{is_active},"expirationDate":{expiration_millis}}}"#
            ),
            signature: String::new(),
            original_id: None,
            jws_representation: None, // Windows doesn't have JWS like iOS/macOS.
        })
    }

    /// No-op: Microsoft Store auto-acknowledges purchases. Method exists for API parity.
    #[allow(clippy::unused_async, clippy::unused_self)]
    pub async fn acknowledge_purchase(&self, _purchase_token: String) -> crate::Result<()> {
        Ok(())
    }

    #[allow(clippy::unused_async)]
    pub async fn consume_purchase(&self, purchase_token: String) -> crate::Result<()> {
        // The opaque token contains the Microsoft product StoreId needed by
        // ReportConsumableFulfillmentAsync.
        let envelope = WindowsPurchaseTokenV1::decode(&purchase_token)?;
        let context = self.get_store_context()?;
        let tracking_id = windows::core::GUID::new()?;

        let result = context
            .ReportConsumableFulfillmentAsync(&HSTRING::from(envelope.store_id), 1u32, tracking_id)
            .and_then(|async_op| async_op.get())?;

        match result.Status()? {
            StoreConsumableStatus::Succeeded => Ok(()),
            StoreConsumableStatus::InsufficentQuantity => Err(Self::reject(
                "insufficientQuantity",
                "Not enough balance remaining to consume",
            )),
            StoreConsumableStatus::NetworkError => {
                Err(Self::reject("networkError", "Network error during consume"))
            }
            StoreConsumableStatus::ServerError => {
                Err(Self::reject("serverError", "Server error during consume"))
            }
            _ => Err(Self::reject("consumeFailed", "Failed to consume purchase")),
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn get_product_status(
        &self,
        product_id: String,
        product_type: String,
    ) -> crate::Result<ProductStatus> {
        // Get app license to check ownership.
        let app_license = self
            .get_store_context()?
            .GetAppLicenseAsync()
            .and_then(|async_op| async_op.get())?;
        // Get add-on licenses (in-app purchases).
        let addon_licenses = app_license.AddOnLicenses()?;

        // Look for the specific product license by developer product id.
        // The AddOnLicenses map key is a SKU StoreId, so do not call Lookup(product_id).
        let iterator = addon_licenses.First()?;
        while iterator.HasCurrent()? {
            let license = iterator.Current()?.Value()?;

            if license.InAppOfferToken()?.to_string() == product_id {
                return Self::convert_license_to_product_status(
                    &license,
                    product_id,
                    &product_type,
                );
            }

            iterator.MoveNext()?;
        }

        Ok(ProductStatus {
            product_id,
            is_owned: false,
            purchase_state: None,
            purchase_time: None,
            expiration_time: None,
            is_auto_renewing: None,
            is_acknowledged: None,
            purchase_token: None,
        })
    }

    fn convert_license_to_product_status(
        license: &StoreLicense,
        product_id: String,
        product_type: &str,
    ) -> crate::Result<ProductStatus> {
        let is_active = license.IsActive()?;
        let expiration_time = Self::datetime_to_unix_millis(license.ExpirationDate()?);
        let sku_store_id = license.SkuStoreId()?.to_string();
        // Keep SKU StoreId internal; expose only an opaque purchase token.
        let store_id = Self::store_id_from_sku_store_id(&sku_store_id);

        let purchase_time = if product_type == "subs" && expiration_time > 0 {
            expiration_time - (30 * 24 * 60 * 60 * 1000)
        } else {
            expiration_time
        };
        let purchase_token = Self::purchase_token_for_store_id(store_id, purchase_time)?;

        let purchase_state = if is_active {
            Some(PurchaseStateValue::Purchased)
        } else {
            Some(PurchaseStateValue::Canceled)
        };

        Ok(ProductStatus {
            product_id,
            is_owned: is_active,
            purchase_state,
            purchase_time: Some(purchase_time),
            expiration_time: (expiration_time > 0).then_some(expiration_time),
            is_auto_renewing: Some(product_type == "subs" && is_active),
            is_acknowledged: Some(true),
            purchase_token: Some(purchase_token),
        })
    }
}
