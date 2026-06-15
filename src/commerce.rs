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

    /// Compute the total discount (in minor units) for `coupon_codes` against
    /// the given basket, via the engine's promotions engine. Returns 0 when
    /// no code matches a configured coupon — which is the case for the
    /// default/un-seeded database, so callers get the prior (no-discount)
    /// behavior unless coupons have actually been provisioned. Unknown or
    /// invalid codes are simply rejected by the engine, never an error.
    pub fn compute_discount_minor(
        &self,
        items: &[crate::models::RequestItem],
        currency: &str,
        ship_to: Option<&crate::models::Address>,
        coupon_codes: &[String],
    ) -> i64 {
        use stateset_core::{ApplyPromotionsRequest, PromotionLineItem};
        let Ok(parsed_currency) = currency.parse() else {
            return 0;
        };
        let mut line_items = Vec::with_capacity(items.len());
        let mut subtotal = Decimal::ZERO;
        for (idx, item) in items.iter().enumerate() {
            let unit_minor = item
                .unit_price_hint
                .as_ref()
                .map(|m| m.amount_minor)
                .unwrap_or(1_000);
            let unit_price = minor_to_decimal(unit_minor, currency);
            let qty = item.quantity.max(0);
            // `rust_decimal` multiply/add PANIC on overflow. A crafted
            // huge amount_minor × quantity would otherwise abort the
            // process (release builds use panic=abort) — the same class the
            // tax path is hardened against. A basket too large to represent
            // simply gets no discount.
            let Some(line_total) = unit_price.checked_mul(Decimal::from(qty)) else {
                return 0;
            };
            let Some(next) = subtotal.checked_add(line_total) else {
                return 0;
            };
            subtotal = next;
            line_items.push(PromotionLineItem {
                id: format!("li_{idx:06}"),
                product_id: None,
                variant_id: None,
                sku: Some(item.sku.clone()),
                category_ids: Vec::new(),
                quantity: qty.min(i64::from(i32::MAX)) as i32,
                unit_price,
                line_total,
            });
        }
        let request = ApplyPromotionsRequest {
            cart_id: None,
            customer_id: None,
            coupon_codes: coupon_codes.to_vec(),
            line_items,
            subtotal,
            shipping_amount: Decimal::ZERO,
            shipping_country: ship_to.and_then(|a| a.country.clone()),
            shipping_state: ship_to.and_then(|a| a.state.clone()),
            currency: parsed_currency,
            is_first_order: false,
        };
        match self.promotions().apply(request) {
            Ok(result) => crate::models::decimal_to_minor(result.total_discount, currency).max(0),
            Err(err) => {
                tracing::warn!(%err, "promotions apply failed; applying no discount");
                0
            }
        }
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

        // The engine maps an unrecognized currency to its default (USD). Log
        // it rather than silently persisting an order in the wrong currency.
        let parsed_currency = currency.parse().ok();
        if parsed_currency.is_none() {
            tracing::warn!(
                currency,
                "currency not recognized by the commerce engine; persisted order will default to its base currency"
            );
        }
        let order = self
            .inner
            .orders()
            .create(CreateOrder {
                customer_id: customer.id,
                items: order_items,
                currency: parsed_currency,
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
