//! Embedded commerce engine (`stateset-icommerce`) wrapper.
//!
//! This is the sole dependency surface the ICP service uses to talk to the
//! engine of record. It deliberately exposes a small, typed façade — the
//! handler reaches through to deeper engine APIs only when a specific
//! intent needs them.

use std::sync::Arc;

use rust_decimal::Decimal;
use stateset_core::{CreateCustomer, CreateOrder, CreateOrderItem};
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
                first_name: buyer.first_name.clone().unwrap_or_default(),
                last_name: buyer.last_name.clone().unwrap_or_default(),
                phone: buyer.phone_number.clone(),
                ..Default::default()
            })
            .map_err(|e| ApiError::EngineUnavailable(format!("customer.create: {e}")))?;

        let order_items = line_items
            .iter()
            .map(|li| CreateOrderItem {
                sku: li.sku.clone(),
                name: li.name.clone(),
                quantity: li.quantity.clamp(0, i32::MAX as i64) as i32,
                unit_price: minor_to_decimal(li.unit_price.amount_minor, currency),
                ..Default::default()
            })
            .collect();

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
