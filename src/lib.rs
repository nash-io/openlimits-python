use pyo3::prelude::*;

use tokio::stream::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use openlimits::{
    exchange::Exchange, 
    exchange_ws::OpenLimitsWs, 
    nash::{Nash, NashStream}, 
    model::{
        OrderBookRequest, 
        OrderBookResponse,
        CancelAllOrdersRequest, 
        CancelOrderRequest,
        OpenLimitOrderRequest,
        Paginator,
        Balance,
        OrderCanceled,
        Order,
        websocket::{Subscription, OpenLimitsWebsocketMessage}
    }
};
use rust_decimal::prelude::{Decimal, FromStr};
use futures_util::future::{select, Either};

#[pyclass]
pub struct NashClient {
    pub handle: tokio::runtime::Handle,
    pub client: Nash,
    pub sub_request_tx: UnboundedSender<Subscription>,
    pub sub_stream_rx: UnboundedReceiver<OpenLimitsWebsocketMessage>,
    rt_shutdown_tx: tokio::sync::oneshot::Sender<()>,
    rt_thread_handle: std::thread::JoinHandle<()>,
}

#[pymethods]
impl NashClient {
    /// Create new client instance from api key secret and session
    #[new]
    pub fn new(secret: &str, session: &str) -> Self {
        // runtime setup logic copied from the C++ wrapper made by https://github.com/MarginUG

        // channel used to shutdown runtime thread
        let (rt_shutdown_tx, rt_shutdown_rx) = tokio::sync::oneshot::channel();
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

                rt_shutdown_rx.await.expect("Tokio runtime shutdown channel had an error");
            });
        });

        // this is handle to tokio runtime that we can use to execute futures in that runtime
        let rt_handle = rt_handle_rx
            .recv()
            .expect("Unable to receive Tokio runtime handle from runtime thread");

        // now we use that handle to initialize the client that we will use for basic requests
        let client_future = Nash::with_credential(secret, session, 0, false, 10000);
        let client = rt_handle.block_on(client_future);

        // unfortunately, due to structure of openlimits we need a seperate client for WS subscriptions
        // here we will set that up. the idea is to launch a new process within the tokio runtime that will continuously
        // poll for either new incoming data on the WS subscription, or something new to send out. it is somewhat unfortunate
        // that we need to do things this way, but again due to openlimits design the same object represents the incoming 
        // stream as well as our handle for sending things out

        // this channel will be used to push subscription data back to the nash client, where we can retrieve it
        // it is an unbounded channel, so messages will accumulate there (in memory) if we don't pull them out fast enough
        let (sub_request_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();
        // this is the channel we will use to pipe subscription requests into the tokio runtime loop, where they can be sent out
        let (incoming_sub_tx, sub_stream_rx) = tokio::sync::mpsc::unbounded_channel();

        // make a copy of the refenced data to move into the thread (todo: clean this up)
        let secret = secret.to_string();
        let session = session.to_string();

        // now we actually spawn the process where we will loop over incoming/outgoing subscription data
        // we use the handle to the tokio runtime, which is itself running inside an independent normal thread
        rt_handle.spawn(async move {
            // openlimits makes us initialize a separate client just for WS
            let ws_client = NashStream::with_credential(&secret, &session, 0, false, 10000).await;
            let mut client = OpenLimitsWs { websocket: ws_client };

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
                    }
                };
            }

        });

        Self {
            handle: rt_handle,
            client,
            sub_request_tx,
            sub_stream_rx,
            rt_shutdown_tx,
            rt_thread_handle
        }
    }

    pub fn subscribe(&self, subscription: Subscription){
        self.sub_request_tx.send(subscription).expect("failed to start subscription");
    }

    pub fn next_subscription_event(&mut self) -> OpenLimitsWebsocketMessage {
        let next_event = self.sub_stream_rx.next();
        let output = self.handle.block_on(next_event);
        output.unwrap()
    }

    pub fn order_book(&self, market_name: &str) -> OrderBookResponse {
        let req = OrderBookRequest {
            market_pair: market_name.to_string()
        };
        let future = self.client.order_book(&req);
        self.handle.block_on(future).unwrap()
    }

    pub fn cancel_all_orders(&self, market_name: Option<&str>) -> Vec<OrderCanceled<String>> {
        let req = CancelAllOrdersRequest {
            market_pair: market_name.map(|x| x.to_string())
        };
        let future = self.client.cancel_all_orders(&req);
        self.handle.block_on(future).unwrap()
    }

    pub fn cancel_order(&self, market_pair: &str, id: &str) -> OrderCanceled<String> {
        // TODO: why is market pair required here?
        let req = CancelOrderRequest {
            market_pair: Some(market_pair.to_string()),
            id:id.to_string()
        };
        let future = self.client.cancel_order(&req);
        self.handle.block_on(future).unwrap()
    }

    pub fn limit_sell(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
        };
        let future = self.client.limit_sell(&req);
        self.handle.block_on(future).unwrap()
    }

    pub fn limit_buy(&self, market_pair: &str, size: &str, price: &str) -> Order<String> {
        let req = OpenLimitOrderRequest {
            market_pair: market_pair.to_string(),
            size: Decimal::from_str(size).unwrap(),
            price: Decimal::from_str(price).unwrap()
        };
        let future = self.client.limit_buy(&req);
        self.handle.block_on(future).unwrap()
    }

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
        self.handle.block_on(future).unwrap()
    }

}

// register with python env
#[pymodule]
fn nash_python(py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<NashClient>()?;
    Ok(())
}

