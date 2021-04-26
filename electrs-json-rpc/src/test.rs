use super::*;

use {
    std::{
        convert::Infallible,
        time::Duration,
    },
    rand::Rng,
    tokio::{
        net::{TcpStream, TcpListener},
    },
    futures::{StreamExt, stream::FuturesUnordered},
    crate::client::ClientCallMethodError,
};

struct TestService;

#[json_rpc_service]
impl TestService {
    #[method = "delay"]
    async fn delay(millis: u64, reply: String) -> Result<String, JsonRpcError> {
        tokio::time::delay_for(Duration::from_millis(millis)).await;
        Ok(reply)
    }
}

json_rpc_client! {
    type TestClient<A> {
        #[method = "delay"]
        async fn delay(&self, millis: u64, reply: &str)
            -> Result<Result<JsonValue, JsonRpcError>, ClientCallMethodError<Infallible>>;
    }
}

#[tokio::test]
async fn spam_lots_of_requests() {
    const NUM_CLIENTS: usize = 1000;
    const MAX_REQUESTS_PER_CLIENT: usize = 100;

    let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let server = {
        let listener_stream = listener.incoming();
        listener_stream
        .take(NUM_CLIENTS)
        .map(|client_res| {
            let client = client_res.expect("error accepting tcp connection");
            let (session, _client) = JsonRpcSession::new(client);
            session.serve(TestService, MAX_REQUESTS_PER_CLIENT)
        })
        .buffer_unordered(NUM_CLIENTS)
        .map(|task_res| task_res.expect("error running client task"))
        .collect::<()>()
    };

    let client_num = std::sync::atomic::AtomicUsize::new(0);
    let clients = FuturesUnordered::new();
    for _ in 0..NUM_CLIENTS {
        let client = async {
            let tcp_stream = TcpStream::connect(local_addr).await.unwrap();
            let (session, client) = JsonRpcSession::new(tcp_stream);
            let task = session.run();
            let client = TestClient::from_inner(client);

            let send_requests = async move {
                let mut rng = rand::thread_rng();
                let num_requests = rng.gen::<usize>() % MAX_REQUESTS_PER_CLIENT;
                for _ in 0..num_requests {
                    let delay = (rng.sample::<f64, _>(rand_distr::Exp1) * 1000.0f64) as u64;
                    let msg = format!("{}", rng.gen::<f64>());
                    let response = client.delay(delay, &msg).await.unwrap().unwrap();
                    assert_eq!(response.as_str().unwrap(), msg);
                }
            };
            let send_requests = send_requests.fuse();
            pin_mut!(send_requests);
            let task = task.fuse();
            pin_mut!(task);

            futures::select! {
                () = send_requests => (),
                result = task => result.unwrap(),
            }

            let clients_finished = {
                1 + client_num.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            };
            println!("finished {}/{} clients", clients_finished, NUM_CLIENTS);
        };
        clients.push(client);
    }
    let clients = clients.for_each(|()| async { () });

    let ((), ()) = futures::join!(server, clients);
}

