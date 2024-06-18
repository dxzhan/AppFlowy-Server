use client_api::entity::PublishItem;
use client_api_test::{generate_unique_registered_user_client, localhost_client};

#[tokio::test]
async fn test_set_publish_namespace_set() {
  let (c, _user) = generate_unique_registered_user_client().await;
  let workspace_id = get_first_workspace_string(&c).await;

  {
    // cannot get namespace if not set
    let err = c
      .get_workspace_publish_namespace(&workspace_id.to_string())
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "PublishNamespaceNotSet");
  }

  let namespace = uuid::Uuid::new_v4().to_string();
  c.set_workspace_publish_namespace(&workspace_id.to_string(), &namespace)
    .await
    .unwrap();

  {
    // cannot set the same namespace
    let err = c
      .set_workspace_publish_namespace(&workspace_id.to_string(), &namespace)
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "PublishNamespaceAlreadyTaken");
  }
  {
    // can replace the namespace
    let namespace = uuid::Uuid::new_v4().to_string();
    c.set_workspace_publish_namespace(&workspace_id.to_string(), &namespace)
      .await
      .unwrap();

    let got_namespace = c
      .get_workspace_publish_namespace(&workspace_id.to_string())
      .await
      .unwrap();
    assert_eq!(got_namespace, namespace);
  }
  {
    // cannot set namespace too short
    let err = c
      .set_workspace_publish_namespace(&workspace_id.to_string(), "a") // too short
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "InvalidRequest");
  }

  {
    // cannot set namespace with invalid chars
    let err = c
      .set_workspace_publish_namespace(&workspace_id.to_string(), "/|(*&)(&#@!") // invalid chars
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "InvalidRequest");
  }
}

#[tokio::test]
async fn test_publish_doc() {
  let (c, _user) = generate_unique_registered_user_client().await;
  let workspace_id = get_first_workspace_string(&c).await;
  let my_namespace = uuid::Uuid::new_v4().to_string();
  c.set_workspace_publish_namespace(&workspace_id.to_string(), &my_namespace)
    .await
    .unwrap();

  // &view_id,
  // my_doc_name,
  // Metadata {
  //   title: "my_title".to_string(),
  // },

  let doc_name1 = "doc1";
  let view_id_1 = uuid::Uuid::new_v4();
  c.publish_collabs::<Metadata>(
    &workspace_id,
    &[PublishItem {
      view_id: view_id_1,
      doc_name: doc_name1.to_string(),
      metadata: Metadata {
        title: "my_title".to_string(),
      },
    }],
  )
  .await
  .unwrap();

  {
    // Non login user should be able to view the published collab
    let guest_client = localhost_client();
    let published_collab = guest_client
      .get_published_collab::<Metadata>(&my_namespace, doc_name1)
      .await
      .unwrap();
    assert_eq!(published_collab.title, "my_title");

    let publish_info = guest_client
      .get_published_collab_info(&view_id_1)
      .await
      .unwrap();
    assert_eq!(publish_info.namespace, Some(my_namespace.clone()));
    assert_eq!(publish_info.doc_name, doc_name1);
    assert_eq!(publish_info.view_id, view_id_1);

    let collab_data = guest_client
      .get_published_collab_blob(&my_namespace, doc_name1)
      .await
      .unwrap();
    assert!(collab_data.is_empty()); // empty data because publisher need to set it
  }

  c.put_published_collab_blob(&workspace_id, &view_id_1, "some_collab_data")
    .await
    .unwrap();

  {
    // Non login user should be able to view the published collab data
    let guest_client = localhost_client();
    let collab_data = guest_client
      .get_published_collab_blob(&my_namespace, doc_name1)
      .await
      .unwrap();
    assert!(collab_data == "some_collab_data");
  }

  c.delete_published_collab(&workspace_id, &view_id_1)
    .await
    .unwrap();

  {
    // Deleted collab should not be accessible
    let guest_client = localhost_client();
    let err = guest_client
      .get_published_collab::<Metadata>(&my_namespace, doc_name1)
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "RecordNotFound");

    let guest_client = localhost_client();
    let err = guest_client
      .get_published_collab_blob(&my_namespace, doc_name1)
      .await
      .err()
      .unwrap();
    assert_eq!(format!("{:?}", err.code), "RecordNotFound");
  }
}

async fn get_first_workspace_string(c: &client_api::Client) -> String {
  c.get_workspaces()
    .await
    .unwrap()
    .0
    .first()
    .unwrap()
    .workspace_id
    .to_string()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Metadata {
  title: String,
}
