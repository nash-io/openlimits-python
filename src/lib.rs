use pyo3::prelude::*;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use std::sync::Arc;
use openlimits::{
    exchange::Exchange, 
    exchange_ws::OpenLimitsWs, 
    exchange_info::MarketPair,
    nash::{Nash, NashStream}, 
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
        websocket::{Subscription, OpenLimitsWebsocketMessage}
    }
};
use rust_decimal::prelude::{Decimal, FromStr};
use futures_util::future::{select, Either, Future};

#[pyclass]
pub struct NashClient {
    pub runtime: ManagedRuntime,
    pub client: Nash,
    pub sub_request_tx: UnboundedSender<Subscription>,
    callback: Arc<Mutex<Option<PyObject>>>
}

pub struct ManagedRuntime {
    runtime_handle: tokio::runtime::Handle,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread_handle: std::thread::JoinHandle<()>
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
        let thread_handle = std::thread::spawn(move || {
            let mut rt = tokio::runtime::Builder::new()
                .threaded_scheduler()
                .core_threads(1)
                .enable_all()
                .build().expect("Could not create Tokio runtime");
            let rt_handle = rt.handle().clone();

            // Continue running until notified to shutdown
            rt.block_on(async move {
                // Share handle with parent thread
                rt_handle_tx
                    .send(rt_handle)
                    .expect("Unable to give Tokio runtime handle to parent thread");

                shutdown_rx.await.expect("Tokio runtime shutdown channel had an error");
            });
        });

        // this is handle to tokio runtime that we can use to execute futures in that runtime
        let runtime_handle = rt_handle_rx
            .recv()
            .expect("Unable to receive Tokio runtime handle from runtime thread");

        Self {
            runtime_handle,
            thread_handle,
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
impl NashClient {
    /// Create new client instance from api key secret and session
    #[new]
    pub fn new(secret: &str, session: &str) -> Self {
        // create a tokio runtime running in a background process that we can communicate with
        let runtime = ManagedRuntime::new();

        // now we use the runtime handle to initialize the client that we will use for basic requests
        let client_future = Nash::with_credential(secret, session, 0, false, 10000);
        let client = runtime.block_on(client_future);

        // this channel will be used to push subscription data back to the nash client, where we can retrieve it
        // it is an unbounded channel, so messages will accumulate there (in memory) if we don't pull them out fast enough
        let (sub_request_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();

        // make a copy of the refenced data to move into the thread (todo: clean this up)
        let secret = secret.to_string();
        let session = session.to_string();

        // This will hold a mutable reference to a future python callback function that we can share across threads
        let callback: Arc<Mutex<Option<PyObject>>> = Arc::new(Mutex::new(None));
        // Make a copy of the callback ref that we can move into the WS runtime loop
        let callback_copy = callback.clone();

        // now we actually spawn the process where we will loop over incoming/outgoing subscription data
        // we use the handle to the tokio runtime, which is itself running inside an independent normal thread
        runtime.spawn(async move {
            // openlimits makes us initialize a separate client just for WS
            let ws_client = NashStream::with_credential(&secret, &session, 0, false, 10000).await;
            let mut client = OpenLimitsWs { websocket: ws_client };

            loop {
                let next_outgoing_sub = sub_rx.next();
                let next_incoming_message = client.next();
                // here we check for either something we need to send out, or something coming in
                match select(next_outgoing_sub, next_incoming_message).await {
                    // if something to send out (new subscription request), then send it
                    Either::Left((sub, _)) => {
                        let sub = sub.unwrap();
                        if let Ok(_) = client.subscribe(sub).await {
                            println!("Subscribed!");
                        } else {
                            println!("Something went wrong...");
                        }
                    },
                    // if something is coming in on the subscription, push it down the callback
                    Either::Right((message, _)) => {
                        let message = message.unwrap().unwrap();
                        // extract the current python callback function if we have one
                        if let Some(py_callback) = callback_copy.lock().await.as_ref() {
                            // move a copy of the callback and execute within a blocking tokio thread
                            let py_callback_copy = py_callback.clone();
                            tokio::task::spawn_blocking(move || {
                                // this takes out lock on Python GIL
                                Python::with_gil(|py| {
                                    let _out = py_callback_copy.call1(py, (message,)).unwrap();
                                });
                            });
                        }
                        
                    }
                };
            }

        });

        Self {
            runtime,
            client,
            sub_request_tx,
            callback
        }
    }

    /// Provide a callable Python object (e.g., a lambda or function) that the WS runtime loop
    /// will execute when a new subscription event is recieved. The function provided should
    /// take one argument, which will be the Python representation of `OpenLimitsWebsocketMessage`
    pub fn set_subscription_callback(&self, pyfn: PyObject){
        let callback_copy = self.callback.clone();
        let set_callback = async move {
            *callback_copy.lock().await = Some(pyfn);
        };
        self.runtime.block_on(set_callback);
    }

    /// Subscribe to any endpoints endpoints supported by openlimits
    pub fn subscribe(&self, subscription: Subscription){
        self.sub_request_tx.send(subscription).expect("failed to start subscription");
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
    pub fn cancel_all_orders(&self, market_name: Option<&str>) -> Vec<OrderCanceled<String>> {
        let req = CancelAllOrdersRequest {
            market_pair: market_name.map(|x| x.to_string())
        };
        let future = self.client.cancel_all_orders(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Cancel a single order
    pub fn cancel_order(&self, market_pair: &str, id: &str) -> OrderCanceled<String> {
        // TODO: why is market pair required here?
        let req = CancelOrderRequest {
            market_pair: Some(market_pair.to_string()),
            id:id.to_string()
        };
        let future = self.client.cancel_order(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Sell limit order
    pub fn limit_sell(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
        };
        let future = self.client.limit_sell(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Buy limit order
    pub fn limit_buy(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
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
    pub fn get_all_open_orders(&self) -> Vec<Order<String>> {
        let future = self.client.get_all_open_orders();
        self.runtime.block_on(future).unwrap()
    }

    /// Get order history
    pub fn get_order_history(&self, market_pair: Option<&str>, paginator: Option<Paginator<String>>) -> Vec<Order<String>> {
        let req = GetOrderHistoryRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            paginator
        };
        let future = self.client.get_order_history(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get historical trade data
    pub fn get_historic_trades(&self, market_pair: &str, paginator: Option<Paginator<String>>) -> Vec<Trade<String, String>>{
        let req = GetHistoricTradesRequest {
            // TODO: why is market required here but not for order history?
            market_pair: market_pair.to_string(),
            paginator
        };
        let future = self.client.get_historic_trades(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get historical price data (candles)
    pub fn get_historic_rates(&self, market_pair: &str, paginator: Option<Paginator<String>>, interval: Interval) -> Vec<Candle>{
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
    pub fn get_order(&self, id: &str, market_pair: Option<&str>) -> Order<String> {
        let req = GetOrderRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            id: id.to_string()
        };
        let future = self.client.get_order(&req);
        self.runtime.block_on(future).unwrap()
    }

    /// Get account trade history
    pub fn get_trade_history(&self, market_pair: Option<&str>, order_id: Option<&str>, paginator: Option<Paginator<String>>) -> Vec<Trade<String, String>> {
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
fn openlimits_python(py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<NashClient>()?;
    Ok(())
}

