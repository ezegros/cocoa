//! Detached credential resolutions share one process-wide, nonqueuing admission limit.

use std::{error::Error, future::poll_fn, pin::pin, task::Poll, time::Duration};

use cocoa_oauth::{OAuthClient, OAuthError, Service, finish_refreshes, mcp::McpOAuthClient};

#[tokio::test]
async fn both_clients_share_64_slots_until_cancelled_waiters_work_finishes()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let subscription_path = directory.path().join("oauth/subscription.json");
    let mcp_path = directory.path().join("oauth/mcp.json");
    let service = Service {
        issuer: "https://issuer.example.test".into(),
        resource: "https://inference.example.test".into(),
        scopes: "use.direct".into(),
        inference_scope: "use.direct".into(),
        registration_client: "registration_entrypoint".into(),
        registration_parameters: Default::default(),
        host_id_parameter: None,
    };
    let subscription = OAuthClient::new(service.clone(), subscription_path.clone())?;
    let mcp = McpOAuthClient::new("https://resource.example.test/mcp", mcp_path.clone(), None)?;
    assert!(matches!(
        subscription.access_token("missing").await,
        Err(OAuthError::Changed)
    ));
    mcp.logout().await?;

    for _ in 0..2 {
        let subscription_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(subscription_path.with_extension("json.lock"))?;
        subscription_lock.try_lock()?;
        let mcp_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(mcp_path.with_extension("json.lock"))?;
        mcp_lock.try_lock()?;

        for index in 0..64 {
            let subscription = OAuthClient::new(service.clone(), subscription_path.clone())?;
            let mcp =
                McpOAuthClient::new("https://resource.example.test/mcp", mcp_path.clone(), None)?;
            let mut waiter = pin!(async {
                if index % 2 == 0 {
                    subscription.access_token("missing").await.map(Some)
                } else {
                    mcp.access_token().await
                }
            });
            poll_fn(|context| {
                assert!(waiter.as_mut().poll(context).is_pending(), "slot {index}");
                Poll::Ready(())
            })
            .await;
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(50), finish_refreshes())
                .await
                .is_err(),
            "cancelled waiters must leave their locked work tracked"
        );
        let mut subscription_waiter = pin!(subscription.access_token("missing"));
        poll_fn(|context| {
            assert!(matches!(
                subscription_waiter.as_mut().poll(context),
                Poll::Ready(Err(OAuthError::Busy))
            ));
            Poll::Ready(())
        })
        .await;
        let mut mcp_waiter = pin!(mcp.access_token());
        poll_fn(|context| {
            assert!(matches!(
                mcp_waiter.as_mut().poll(context),
                Poll::Ready(Err(OAuthError::Busy))
            ));
            Poll::Ready(())
        })
        .await;

        drop(subscription_lock);
        drop(mcp_lock);
        tokio::time::timeout(Duration::from_secs(5), finish_refreshes()).await?;
        assert!(matches!(
            subscription.access_token("missing").await,
            Err(OAuthError::Changed)
        ));
        assert!(mcp.access_token().await?.is_none());
    }
    Ok(())
}
