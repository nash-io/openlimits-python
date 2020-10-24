use pyo3::prelude::*;

use tokio::stream::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use std::sync::Arc;
use tokio::sync::Mutex;
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
use futures_util::future::{select, Either};

#[pyclass]
pub struct NashClient {
    pub request_rt_handle: tokio::runtime::Handle,
    request_rt_shutdown_tx: tokio::sync::oneshot::Sender<()>,
    request_thread_handle: std::thread::JoinHandle<()>,
    pub client: Nash,
    pub sub_request_tx: UnboundedSender<Subscription>,
    pub sub_stream_rx: UnboundedReceiver<OpenLimitsWebsocketMessage>,
    pub py_callback_tx: UnboundedSender<PyObject>,
}

// setup a tokio runtime in a new thread and get back handle to thread and runtime
// logic borrowed with small changes from the C++ wrapper made by https://github.com/MarginUG
pub fn launch_runtime(
    shutdown_rx: tokio::sync::oneshot::Receiver<()> 
) -> (std::thread::JoinHandle<()>, tokio::runtime::Handle) {
    // channel we will use to send runtime handle back from runtime thread
    let (rt_handle_tx, rt_handle_rx) = std::sync::mpsc::channel();

    // launch the runtime thread. we will get handle to tokio runtime back on the channel
    let rt_thread_handle = std::thread::spawn(move || {
        println!("Creating Tokio runtime");
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
    let request_rt_handle = rt_handle_rx
        .recv()
        .expect("Unable to receive Tokio runtime handle from runtime thread");

    // return handle to thread and tokio runtime
    (rt_thread_handle, request_rt_handle)
}

#[pymethods]
impl NashClient {
    /// Create new client instance from api key secret and session
    #[new]
    pub fn new(secret: &str, session: &str) -> Self {

        // channel used to shutdown request runtime thread
        let (request_rt_shutdown_tx, request_rt_shutdown_rx) = tokio::sync::oneshot::channel();
        // launch the request runtime thread and get thread and runtime handles
        let (request_thread_handle, request_rt_handle) = launch_runtime(request_rt_shutdown_rx);

        // now we use the runtime handle to initialize the client that we will use for basic requests
        let client_future = Nash::with_credential(secret, session, 0, false, 10000);
        let client = request_rt_handle.block_on(client_future);

        // this channel will be used to push subscription data back to the nash client, where we can retrieve it
        // it is an unbounded channel, so messages will accumulate there (in memory) if we don't pull them out fast enough
        let (sub_request_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();
        // this is the channel we will use to pipe subscription requests into the tokio runtime loop, where they can be sent out
        let (incoming_sub_tx, sub_stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (py_callback_tx, mut py_callback_rx) = tokio::sync::mpsc::unbounded_channel::<PyObject>();

        // make a copy of the refenced data to move into the thread (todo: clean this up)
        let secret = secret.to_string();
        let session = session.to_string();

        // now we actually spawn the process where we will loop over incoming/outgoing subscription data
        // we use the handle to the tokio runtime, which is itself running inside an independent normal thread
        request_rt_handle.spawn(async move {
            // openlimits makes us initialize a separate client just for WS
            let ws_client = NashStream::with_credential(&secret, &session, 0, false, 10000).await;
            let mut client = OpenLimitsWs { websocket: ws_client };

            let py_callback = py_callback_rx.next().await.unwrap();

            loop {

                let next_outgoing_sub = sub_rx.next();
                let next_incoming_message = client.next();
                // here we check for either somethign we need to send out, or something coming in
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
                    // if something is coming in on the subscription, push it down the channel to client
                    Either::Right((message, _)) => {
                        let message = message.unwrap().unwrap();
                        incoming_sub_tx.send(message).expect("failed to send incoming message down channel");
                        let py_callback_copy = py_callback.clone();
                        tokio::task::spawn_blocking(move || {
                            Python::with_gil(|py| {
                                let _out = py_callback_copy.call1(py, ()).unwrap();
                            });
                        });
                    }
                };
            }

        });

        Self {
            request_rt_handle,
            client,
            sub_request_tx,
            sub_stream_rx,
            py_callback_tx,
            request_rt_shutdown_tx,
            request_thread_handle,
        }
    }

    /// Subscribe any endpoints endpoints supported by openlimits
    pub fn subscribe(&self, subscription: Subscription, pyfn: PyObject){
        self.sub_request_tx.send(subscription).expect("failed to start subscription");
        self.py_callback_tx.send(pyfn).expect("could not send python callback");
    }

    /// Blocking call to read subscription data from a global stream
    pub fn next_subscription_event(&mut self) -> OpenLimitsWebsocketMessage {
        let next_event = self.sub_stream_rx.next();
        let output = self.request_rt_handle.block_on(next_event);
        output.unwrap()
    }

    /// Request current order book state
    pub fn order_book(&self, market_name: &str) -> OrderBookResponse {
        let req = OrderBookRequest {
            market_pair: market_name.to_string()
        };
        let future = self.client.order_book(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Cancel all orders, with optional market filter
    pub fn cancel_all_orders(&self, market_name: Option<&str>) -> Vec<OrderCanceled<String>> {
        let req = CancelAllOrdersRequest {
            market_pair: market_name.map(|x| x.to_string())
        };
        let future = self.client.cancel_all_orders(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Cancel a single order
    pub fn cancel_order(&self, market_pair: &str, id: &str) -> OrderCanceled<String> {
        // TODO: why is market pair required here?
        let req = CancelOrderRequest {
            market_pair: Some(market_pair.to_string()),
            id:id.to_string()
        };
        let future = self.client.cancel_order(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Sell limit order
    pub fn limit_sell(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
        };
        let future = self.client.limit_sell(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Buy limit order
    pub fn limit_buy(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
        };
        let future = self.client.limit_buy(&req);
        self.request_rt_handle.block_on(future).unwrap()
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
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// List markets on exchange
    pub fn list_markets(&self) -> Vec<MarketPair> {
        // Todo: this should be fixed in openlimits
        let future = self.client.refresh_market_info();
        let out = self.request_rt_handle.block_on(future).unwrap();
        out.iter().map(|x| x.inner.read().unwrap().clone()).collect()
    }

    /// Get all open orders
    pub fn get_all_open_orders(&self) -> Vec<Order<String>> {
        let future = self.client.get_all_open_orders();
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get order history
    pub fn get_order_history(&self, market_pair: Option<&str>, paginator: Option<Paginator<String>>) -> Vec<Order<String>> {
        let req = GetOrderHistoryRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            paginator
        };
        let future = self.client.get_order_history(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get historical trade data
    pub fn get_historic_trades(&self, market_pair: &str, paginator: Option<Paginator<String>>) -> Vec<Trade<String, String>>{
        let req = GetHistoricTradesRequest {
            // TODO: why is market required here but not for order history?
            market_pair: market_pair.to_string(),
            paginator
        };
        let future = self.client.get_historic_trades(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get historical price data (candles)
    pub fn get_historic_rates(&self, market_pair: &str, paginator: Option<Paginator<String>>, interval: Interval) -> Vec<Candle>{
        let req = GetHistoricRatesRequest {
            market_pair: market_pair.to_string(),
            paginator,
            interval
        };
        let future = self.client.get_historic_rates(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get ticker
    pub fn get_ticker(&self, market_pair: &str) -> Ticker {
        let req = GetPriceTickerRequest {
            market_pair: market_pair.to_string()
        };
        let future = self.client.get_price_ticker(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get order by id
    pub fn get_order(&self, id: &str, market_pair: Option<&str>) -> Order<String> {
        let req = GetOrderRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            id: id.to_string()
        };
        let future = self.client.get_order(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

    /// Get account trade history
    pub fn get_trade_history(&self, market_pair: Option<&str>, order_id: Option<&str>, paginator: Option<Paginator<String>>) -> Vec<Trade<String, String>> {
        let req = TradeHistoryRequest {
            market_pair: market_pair.map(|x| x.to_string()),
            order_id: order_id.map(|x| x.to_string()),
            paginator
        };
        let future = self.client.get_trade_history(&req);
        self.request_rt_handle.block_on(future).unwrap()
    }

}

// register with python env
#[pymodule]
fn openlimits_python(py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<NashClient>()?;
    Ok(())
}

