use pyo3::prelude::*;
use openlimits::{
    OpenLimits,
    exchange::traits::*,
    exchange::any::*,
    exchange::traits::info::*,
    exchange::shared::Result as OpenLimitsResult,
    model::{
        OrderBookRequest, 
        OrderBookResponse,
        CancelAllOrdersRequest, 
        CancelOrderRequest,
        OpenLimitOrderRequest,
        GetOrderHistoryRequest,
        GetOrderRequest,
        TradeHistoryRequest,
        GetHistoricTradesRequest,
        GetHistoricRatesRequest,
        GetPriceTickerRequest,
        Paginator,
        Balance,
        OrderCanceled,
        Order,
        Trade,
        Interval,
        Candle,
        Ticker,
        TimeInForce,
        websocket::Subscription,
        websocket::{WebSocketResponse, OpenLimitsWebSocketMessage}
    }
};
use rust_decimal::prelude::{Decimal, FromStr};
use futures_util::future::Future;
use openlimits::exchange::traits::stream::OpenLimitsWs;

#[pyclass]
pub struct ExchangeClient {
    pub runtime: ManagedRuntime,
    pub client: AnyExchange,
    pub ws_client: OpenLimitsWs<AnyWsExchange>
}

pub struct ManagedRuntime {
    runtime_handle: tokio::runtime::Handle,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _thread_handle: std::thread::JoinHandle<()>
}

impl ManagedRuntime {
    /// Create a new tokio runtime running in another thread that we can communicate with
    pub fn new() -> Self {
        // channel to manage shutting down the runtime
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        // channel we will use to send runtime handle back from runtime thread
        let (rt_handle_tx, rt_handle_rx) = std::sync::mpsc::channel();

        // launch the runtime thread. we will get handle to tokio runtime back on the channel
        // thread setup logic borrowed with small changes from the C++ wrapper made by https://github.com/MarginU
        let _thread_handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build().expect("Could not create Tokio runtime");
            let rt_handle = rt.handle().clone();

            // Continue running until notified to shutdown
            rt.block_on(async move {
                // Share handle with parent thread
                rt_handle_tx
                    .send(rt_handle)
                    .expect("Unable to give Tokio runtime handle to parent thread");

                // We wait for a message from the sender. If it fails, it means the sender was terminated,
                // so we no longer need to wait for it.
                shutdown_rx.await.ok();
            });
        });

        // this is handle to tokio runtime that we can use to execute futures in that runtime
        let runtime_handle = rt_handle_rx
            .recv()
            .expect("Unable to receive Tokio runtime handle from runtime thread");

        Self {
            runtime_handle,
            _thread_handle,
            shutdown_tx: Some(shutdown_tx)
        }
    }
    /// Execute future in the managed runtime (wrapper over tokio)
    pub fn block_on<F: Future>(&self, future: F) -> F::Output{
        self.runtime_handle.block_on(future)
    }
    // Spawn process within managed runtime (wrapper over tokio) 
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where 
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime_handle.spawn(future)
    }
    /// Notify to shutdown runtime inside thread
    pub fn shutdown(&mut self){
        // we need ownership of the shutdown sender channel
        let shutdown_tx = std::mem::replace(&mut self.shutdown_tx, None);
        shutdown_tx.expect("Nothing to shutdown").send(()).expect("Something went wrong shutting down");
    }
}



#[pymethods]
impl ExchangeClient {
    /// Create new client instance from api key secret and session
    #[new]
    pub fn new(init_params: InitAnyExchange) -> Self {
        // create a tokio runtime running in a background process that we can communicate with
        let runtime = ManagedRuntime::new();

        let client_future = OpenLimits::instantiate(init_params.clone());
        let client = runtime.block_on(client_future).expect("Couldn't create Client.");

        let ws_client_future = OpenLimitsWs::instantiate(init_params);
        let ws_client: OpenLimitsWs<AnyWsExchange> = runtime.block_on(ws_client_future).expect("Couldn't create WebSocket Client.");

        Self {
            runtime,
            client,
            ws_client,
        }
    }

    /// Subscribe to any endpoints endpoints supported by openlimits
    pub fn subscribe(&self, subscription: Subscription, pyfn: PyObject) {
        let callback = Box::new(move |message: &OpenLimitsResult<WebSocketResponse<OpenLimitsWebSocketMessage>>| {
            let py_callback_copy = pyfn.clone();
            if let Ok(WebSocketResponse::Generic(message)) = message {
                let message = message.clone();
                tokio::task::spawn_blocking(move || {
                    // this takes out lock on Python GIL
                    Python::with_gil(|py| {
                        let _out = py_callback_copy.call1(py, (message,)).unwrap();
                    });
                });
            }
        });
        let sub_future = self.ws_client.subscribe(subscription, callback);
        self.runtime.block_on(sub_future).unwrap();
    }

    /// Request current order book state
    pub fn order_book(&self, market_name: &str) -> OrderBookResponse {
        let req = OrderBookRequest {
            market_pair: market_name.to_string()
        };
        let future = self.client.order_book(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Cancel all orders, with optional market filter
    pub fn cancel_all_orders(&self, market_name: Option<&str>) -> Vec<OrderCanceled> {
        let req = CancelAllOrdersRequest {
            market_pair: market_name.map(|x| x.to_string())
        };
        let future = self.client.cancel_all_orders(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Cancel a single order
    pub fn cancel_order(&self, market_pair: &str, id: &str) -> OrderCanceled {
        // TODO: why is market pair required here?
        let req = CancelOrderRequest {
            market_pair: Some(market_pair.to_string()),
            id:id.to_string()
        };
        let future = self.client.cancel_order(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Sell limit order
    pub fn limit_sell(&self, market_pair: &str, size: &str, price: &str, time_in_force: TimeInForce, post_only: bool) -> Order {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap(),
            post_only,
            time_in_force
        };
        let future = self.client.limit_sell(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Buy limit order
    pub fn limit_buy(&self, market_pair: &str, size: &str, price: &str, time_in_force: TimeInForce, post_only: bool) -> Order {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap(),
            post_only,
            time_in_force
        };
        let future = self.client.limit_buy(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get balances for current account session
    pub fn get_account_balances(&self, page: Option<String>) -> Vec<Balance> {
        let page = page.map(|x| {
            // TODO: is this correct?
            Paginator {
                before: Some(x),
                after: None,
                limit: None,
                start_time: None,
                end_time: None
            }
        });
        let future = self.client.get_account_balances(page);
        self.runtime.block_on(future).unwrap()
    }

    /// List markets on exchange
    pub fn list_markets(&self) -> Vec<MarketPair> {
        // Todo: this should be fixed in openlimits
        let future = self.client.refresh_market_info();
        let out = self.runtime.block_on(future).unwrap();
        out.iter().map(|x| x.inner.read().unwrap().clone()).collect()
    }

    /// Get all open orders
    pub fn get_all_open_orders(&self) -> Vec<Order> {
        let future = self.client.get_all_open_orders();
        self.runtime.block_on(future).unwrap()
    }

    /// Get order history
    pub fn get_order_history(&self, market_pair: Option<&str>, paginator: Option<Paginator>) -> Vec<Order> {
        let req = GetOrderHistoryRequest {
            order_status: None,
            market_pair: market_pair.map(|x| x.to_string()),
            paginator
        };
        let future = self.client.get_order_history(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get historical trade data
    pub fn get_historic_trades(
        &self, 
        market_pair: &str, 
        paginator: Option<Paginator>
    ) -> Vec<Trade>
    {
        let req = GetHistoricTradesRequest {
            // TODO: why is market required here but not for order history?
            market_pair: market_pair.to_string(),
            paginator
        };
        let future = self.client.get_historic_trades(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get historical price data (candles)
    pub fn get_historic_rates(&self, market_pair: &str, paginator: Option<Paginator>, interval: Interval) -> Vec<Candle>{
        let req = GetHistoricRatesRequest {
            market_pair: market_pair.to_string(),
            paginator,
            interval
        };
        let future = self.client.get_historic_rates(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get ticker
    pub fn get_ticker(&self, market_pair: &str) -> Ticker {
        let req = GetPriceTickerRequest {
            market_pair: market_pair.to_string()
        };
        let future = self.client.get_price_ticker(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get order by id
    pub fn get_order(&self, id: &str, market_pair: Option<&str>) -> Order {
        let req = GetOrderRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            id: id.to_string()
        };
        let future = self.client.get_order(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get account trade history
    pub fn get_trade_history(&self, market_pair: Option<&str>, order_id: Option<&str>, paginator: Option<Paginator>) -> Vec<Trade> {
        let req = TradeHistoryRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            order_id: order_id.map(|x| x.to_string()),
            paginator
        };
        let future = self.client.get_trade_history(&req);
        self.runtime.block_on(future).unwrap()
    }

}

// register with python env
#[pymodule]
fn openlimits_python(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<ExchangeClient>()?;
    Ok(())
}

