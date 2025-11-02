use tokio::join;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = gyre::channel::<i32>(2);

    let mut rx_cloned = rx.clone();
    let handle1 = tokio::spawn(async move {
        while let Some(s) = rx_cloned.next().await {
            println!("1: {:?}", *s);
        }
    });

    let handle2 = tokio::spawn(async move {
        while let Some(s) = rx.next().await {
            println!("2: {:?}", *s);
        }
    });

    let tx_cloned = tx.clone();
    let handle3 = tokio::spawn(async move {
        for i in (1..32) {
            tx_cloned.publish(i).await;
        }
    });

    let handle4 = tokio::spawn(async move {
        for i in (32..64) {
            tx.publish(i).await;
        }
    });

    let _ = join!(handle1, handle2, handle3, handle4);
}
