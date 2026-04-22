//! Embedded commerce engine (`stateset-icommerce`) wrapper.
//!
//! This is the sole dependency surface the ICP service uses to talk to the
//! engine of record. It deliberately exposes a small, typed façade — the
//! handler reaches through to deeper engine APIs only when a specific
//! intent needs them.

use std::sync::Arc;

use rust_decimal::Decimal;
use stateset_core::{
    CreateCustomer, CreateOrder, CreateOrderItem, CreateProduct, CreateProductVariant, ProductId,
};
use stateset_embedded::Commerce;

use crate::errors::ApiError;
use crate::models::{Buyer, LineItem, Money, Totals};

/// Thread-safe handle to an embedded iCommerce engine.
#[derive(Clone)]
pub struct CommerceEngine {
    inner: Arc<Commerce>,
}

impl CommerceEngine {
    /// Open (or create) an embedded commerce database at `path`.
    ///
    /// Use `:memory:` for ephemeral/in-memory stores (tests). Pointing at a
    /// `postgres://` URL requires the `postgres` feature.
    pub fn open(path: &str) -> Result<Self, ApiError> {
        let commerce = if is_postgres_url(path) {
            #[cfg(feature = "postgres")]
            {
                Commerce::with_postgres(path)
                    .map_err(|e| ApiError::EngineUnavailable(e.to_string()))?
            }
            #[cfg(not(feature = "postgres"))]
            {
                return Err(ApiError::EngineUnavailable(
                    "postgres feature not enabled for iCommerce".into(),
                ));
            }
        } else {
            Commerce::new(path).map_err(|e| ApiError::EngineUnavailable(e.to_string()))?
        };
        Ok(Self {
            inner: Arc::new(commerce),
        })
    }

    pub fn products(&self) -> stateset_embedded::Products {
        self.inner.products()
    }

    pub fn inventory(&self) -> stateset_embedded::Inventory {
        self.inner.inventory()
    }

    pub fn customers(&self) -> stateset_embedded::Customers {
        self.inner.customers()
    }

    pub fn orders(&self) -> stateset_embedded::Orders {
        self.inner.orders()
    }

    pub fn promotions(&self) -> stateset_embedded::Promotions {
        self.inner.promotions()
    }

    pub fn tax(&self) -> stateset_embedded::Tax {
        self.inner.tax()
    }

    /// Persist a completed ICP transaction as a real order in the engine.
    ///
    /// Returns `Ok(Some(order_number))` when the engine accepts the order,
    /// `Ok(None)` when there is not enough buyer identity to create a
    /// customer (the engine requires `email` on `CreateCustomer`), or
    /// `Err` when the engine call fails.
    ///
    /// The engine validates:
    ///   * `customer.first_name` and `email` are non-empty.
    ///   * Every order item carries a non-nil `product_id` that resolves
    ///     to a real product in the catalog.
    ///
    /// So for each line item we first look up the product by SKU and
    /// auto-create (`products().create`) with a single default variant
    /// when the catalog doesn't know the SKU yet. This makes the ICP
    /// quote→buy flow self-seeding against a fresh engine database —
    /// useful for demos and conformance runs where no pre-loaded
    /// catalog exists.
    pub fn persist_order(
        &self,
        buyer: &Buyer,
        currency: &str,
        line_items: &[LineItem],
        totals: &Totals,
    ) -> Result<Option<PersistedOrder>, ApiError> {
        let Some(email) = buyer.email.as_ref().filter(|s| !s.is_empty()) else {
            return Ok(None);
        };

        let customer = self
            .inner
            .customers()
            .create(CreateCustomer {
                email: email.clone(),
                // Engine rejects empty first_name; fall back to the local
                // part of the email, then to a sentinel.
                first_name: non_empty(buyer.first_name.as_deref())
                    .unwrap_or_else(|| local_part(email).unwrap_or("ICP Buyer").to_string()),
                last_name: buyer.last_name.clone().unwrap_or_default(),
                phone: buyer.phone_number.clone(),
                ..Default::default()
            })
            .map_err(|e| ApiError::EngineUnavailable(format!("customer.create: {e}")))?;

        // Resolve each SKU to a real product_id, auto-creating missing
        // ones on the fly.
        let mut order_items = Vec::with_capacity(line_items.len());
        for li in line_items {
            let unit_price = minor_to_decimal(li.unit_price.amount_minor, currency);
            let product_id = self.ensure_product_for_sku(&li.sku, &li.name, unit_price)?;
            order_items.push(CreateOrderItem {
                product_id,
                sku: li.sku.clone(),
                name: li.name.clone(),
                quantity: li.quantity.clamp(1, i32::MAX as i64) as i32,
                unit_price,
                ..Default::default()
            });
        }

        let order = self
            .inner
            .orders()
            .create(CreateOrder {
                customer_id: customer.id,
                items: order_items,
                currency: currency.parse().ok(),
                ..Default::default()
            })
            .map_err(|e| ApiError::EngineUnavailable(format!("orders.create: {e}")))?;

        Ok(Some(PersistedOrder {
            id: order.id.to_string(),
            order_number: order.order_number,
            total: totals
                .total
                .clone()
                .unwrap_or_else(|| Money::new(0, currency)),
        }))
    }

    /// Look up a product by its variant SKU, or create a product +
    /// default variant and return the new product id.
    fn ensure_product_for_sku(
        &self,
        sku: &str,
        name: &str,
        unit_price: Decimal,
    ) -> Result<ProductId, ApiError> {
        match self.inner.products().get_variant_by_sku(sku) {
            Ok(Some(variant)) => return Ok(variant.product_id),
            Ok(None) => {}
            Err(e) => {
                return Err(ApiError::EngineUnavailable(format!(
                    "products.get_variant_by_sku({sku}): {e}"
                )));
            }
        }

        let display_name = non_empty(Some(name)).unwrap_or_else(|| sku.to_string());
        let product = self
            .inner
            .products()
            .create(CreateProduct {
                name: display_name.clone(),
                description: Some(format!("Auto-created by ICP handler for SKU {sku}")),
                variants: Some(vec![CreateProductVariant {
                    sku: sku.to_string(),
                    name: Some(display_name),
                    price: unit_price,
                    compare_at_price: None,
                    cost: None,
                    barcode: None,
                    weight: None,
                    weight_unit: None,
                    options: None,
                    is_default: Some(true),
                }]),
                ..Default::default()
            })
            .map_err(|e| ApiError::EngineUnavailable(format!("products.create({sku}): {e}")))?;
        Ok(product.id)
    }
}

pub struct PersistedOrder {
    pub id: String,
    pub order_number: String,
    pub total: Money,
}

fn minor_to_decimal(amount_minor: i64, currency: &str) -> Decimal {
    let scale = crate::models::minor_unit_scale(currency);
    Decimal::from(amount_minor) / Decimal::from(scale.max(1))
}

fn is_postgres_url(s: &str) -> bool {
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn local_part(email: &str) -> Option<&str> {
    email
        .split_once('@')
        .map(|(local, _)| local)
        .filter(|s| !s.is_empty())
}
