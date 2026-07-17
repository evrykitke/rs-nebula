//! Proof of concept: the full authentication story against a live
//! database — company registration creating the tenant admin, login,
//! lockout, profile-opt-in TOTP with an authenticator app, recovery
//! codes, company-mandated two-factor setup, and the company password
//! policy (character rules, reuse history, and the expiry that forces a
//! change before a session is issued).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nebula::auth::{NewUser, UserManager, totp};
use nebula::config::{Config, DatabaseConfig};
use nebula::{AdministrationModule, Kernel, db};
use tower::ServiceExt;

async fn post_json(
    router: &Router,
    path: &str,
    tenant: Option<&str>,
    bearer: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(name) = tenant {
        req = req.header("X-Tenant", name);
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn put_json(
    router: &Router,
    path: &str,
    tenant: Option<&str>,
    bearer: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::put(path).header("content-type", "application/json");
    if let Some(name) = tenant {
        req = req.header("X-Tenant", name);
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_json(
    router: &Router,
    path: &str,
    tenant: Option<&str>,
    bearer: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::get(path);
    if let Some(name) = tenant {
        req = req.header("X-Tenant", name);
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn authentication_end_to_end() {
    let Ok(url) = std::env::var("NEBULA_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set NEBULA_TEST_DATABASE_URL to run database tests");
        return;
    };

    let admin_db = db::connect(&DatabaseConfig {
        url: url.as_str().into(),
        ..DatabaseConfig::default()
    })
    .await
    .expect("must connect");
    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        "DROP TABLE IF EXISTS user_directory; DROP TABLE IF EXISTS currencies; DROP TABLE IF EXISTS audit_logs; DROP TABLE IF EXISTS permission_grants; \
         DROP TABLE IF EXISTS user_roles; DROP TABLE IF EXISTS roles; \
         DROP TABLE IF EXISTS password_history; DROP TABLE IF EXISTS tenant_mail_settings; \
         DROP TABLE IF EXISTS users; DROP TABLE IF EXISTS tenants; \
         DROP TABLE IF EXISTS nebula_migrations;",
    )
    .await
    .expect("cleanup must work");

    let mut config = Config::default();
    config.database = DatabaseConfig {
        url: url.as_str().into(),
        auto_migrate: true,
        ..DatabaseConfig::default()
    };
    config.multitenancy.enabled = true;
    // This test asserts against the main database; no per-tenant database.
    config.multitenancy.provision_databases = false;
    config.multitenancy.allow_shared_database = true;
    config.auth.jwt_secret = "test-secret-not-for-production".into();
    config.auth.lockout_max_failed = 3;
    config.security.encryption_key = "test-encryption-key-not-for-production".into();
    let auth_config = config.auth.clone();

    let app = Kernel::builder()
        .with_config(config)
        .add_module(AdministrationModule)
        .build()
        .expect("kernel must build")
        .init()
        .await
        .expect("boot must succeed");
    let router = app.router();

    // -- A company registers; email + password become the admin account.
    let (status, body) = post_json(
        &router,
        "/auth/register",
        None,
        None,
        serde_json::json!({
            "tenant_name": "acme",
            "company_display_name": "Acme Ltd",
            "email": "boss@acme.test",
            "password": "hunter2hunter2",
            "first_name": "Ada",
            "last_name": "Boss",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    assert_eq!(body["user"]["is_tenant_admin"], true);
    assert_eq!(body["user"]["two_factor_enabled"], false);
    let tenant_id = uuid::Uuid::parse_str(body["tenant_id"].as_str().unwrap()).unwrap();

    // Duplicate company name is a conflict.
    let (status, _) = post_json(
        &router,
        "/auth/register",
        None,
        None,
        serde_json::json!({
            "tenant_name": "acme",
            "email": "other@acme.test",
            "password": "hunter2hunter2",
            "first_name": "O",
            "last_name": "Ther",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A bad admin account (short password) fails validation BEFORE the
    // tenant is created — the name is not burned, so a corrected retry
    // succeeds. Without the pre-check this stranded an admin-less tenant.
    let bad_then_good = |password: &str| {
        serde_json::json!({
            "tenant_name": "initech",
            "email": "boss@initech.test",
            "password": password,
            "first_name": "I",
            "last_name": "Nitech",
        })
    };
    let (status, _) = post_json(&router, "/auth/register", None, None, bad_then_good("x")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = post_json(
        &router,
        "/auth/register",
        None,
        None,
        bad_then_good("hunter2hunter2"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a failed registration must not burn the tenant name: {body}"
    );

    // -- No tenant header needed: sign-in resolves the tenant from the
    // credentials via the login directory.
    let (status, body) = post_json(
        &router,
        "/auth/login",
        None,
        None,
        serde_json::json!({ "login": "boss@acme.test", "password": "hunter2hunter2" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolved login failed: {body}");
    assert_eq!(body["status"], "success");
    assert_eq!(body["tenant"], "acme");

    // A second company with the same admin email makes the login
    // ambiguous: the user is offered the choice and retries with the
    // tenant header.
    let (status, body) = post_json(
        &router,
        "/auth/register",
        None,
        None,
        serde_json::json!({
            "tenant_name": "globex",
            "company_display_name": "Globex Corp",
            "email": "boss@acme.test",
            "password": "hunter2hunter2",
            "first_name": "Ada",
            "last_name": "Boss",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second register failed: {body}");

    let (status, body) = post_json(
        &router,
        "/auth/login",
        None,
        None,
        serde_json::json!({ "login": "boss@acme.test", "password": "hunter2hunter2" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "tenant_selection", "got: {body}");
    let names: Vec<&str> = body["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"acme") && names.contains(&"globex"));

    let (status, body) = post_json(
        &router,
        "/auth/login",
        Some("globex"),
        None,
        serde_json::json!({ "login": "boss@acme.test", "password": "hunter2hunter2" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "success");
    assert_eq!(body["tenant"], "globex");

    // -- Login: wrong password is 401 and eventually locks the account.
    let login =
        |password: &str| serde_json::json!({ "login": "boss@acme.test", "password": password });
    for _ in 0..3 {
        let (status, _) =
            post_json(&router, "/auth/login", Some("acme"), None, login("wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        login("hunter2hunter2"),
    )
    .await;
    assert_eq!(status, StatusCode::LOCKED, "expected lockout: {body}");

    // Clear the lockout and log in properly.
    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        "UPDATE users SET lockout_end_at = NULL WHERE email = 'boss@acme.test'",
    )
    .await
    .unwrap();
    let (status, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        login("hunter2hunter2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "success");
    let access = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = get_json(&router, "/auth/me", Some("acme"), Some(&access)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], "boss@acme.test");

    // A two-factor bridge token must NOT work as an access token, and
    // requests without a token are rejected.
    let (status, _) = get_json(&router, "/auth/me", Some("acme"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // -- Profile opt-in 2FA with an authenticator app.
    let (status, body) = post_json(
        &router,
        "/auth/two-factor/setup",
        Some("acme"),
        Some(&access),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "setup failed: {body}");
    let secret = body["secret"].as_str().unwrap().to_string();
    assert!(
        body["otpauth_url"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/")
    );

    let code = totp::current_code(&secret).unwrap();
    let (status, body) = post_json(
        &router,
        "/auth/two-factor/confirm",
        Some("acme"),
        Some(&access),
        serde_json::json!({ "code": code }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirm failed: {body}");
    let recovery: Vec<String> = body["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(recovery.len(), totp::RECOVERY_CODE_COUNT);

    // Old access token died with the security-stamp rotation.
    let (status, _) = get_json(&router, "/auth/me", Some("acme"), Some(&access)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // -- Login now requires the authenticator.
    let (status, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        login("hunter2hunter2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "two_factor_required");
    let bridge = body["two_factor_token"].as_str().unwrap().to_string();

    // The bridge token is not an access token.
    let (status, _) = get_json(&router, "/auth/me", Some("acme"), Some(&bridge)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let code = totp::current_code(&secret).unwrap();
    let (status, body) = post_json(
        &router,
        "/auth/login/two-factor",
        Some("acme"),
        Some(&bridge),
        serde_json::json!({ "code": code }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "2fa login failed: {body}");
    assert_eq!(body["status"], "success");
    let access = body["access_token"].as_str().unwrap().to_string();

    // -- Recovery codes: single use.
    let (_, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        login("hunter2hunter2"),
    )
    .await;
    let bridge2 = body["two_factor_token"].as_str().unwrap().to_string();
    let (status, _) = post_json(
        &router,
        "/auth/login/two-factor",
        Some("acme"),
        Some(&bridge2),
        serde_json::json!({ "code": recovery[0] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "recovery code must work once");

    let (_, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        login("hunter2hunter2"),
    )
    .await;
    let bridge3 = body["two_factor_token"].as_str().unwrap().to_string();
    let (status, _) = post_json(
        &router,
        "/auth/login/two-factor",
        Some("acme"),
        Some(&bridge3),
        serde_json::json!({ "code": recovery[0] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "recovery code must not be reusable"
    );

    // -- The company mandates 2FA for everyone.
    let (status, body) = post_json(
        &router,
        "/auth/tenant/two-factor",
        Some("acme"),
        Some(&access),
        serde_json::json!({ "required": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "policy toggle failed: {body}");
    assert_eq!(body["require_two_factor"], true);

    // A fresh employee without an authenticator is forced through setup.
    let users = UserManager::new(admin_db.clone(), auth_config);
    users
        .create(NewUser {
            tenant_id: Some(tenant_id),
            user_name: "emp".into(),
            email: "emp@acme.test".into(),
            password: "employeepass1".into(),
            first_name: "Eve".into(),
            last_name: "Mployee".into(),
            is_tenant_admin: false,
            language: None,
            time_zone: None,
            phone_number: None,
        })
        .await
        .expect("employee must be created");

    let (status, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        serde_json::json!({ "login": "emp", "password": "employeepass1" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "two_factor_setup_required", "got: {body}");
    let setup_bridge = body["two_factor_token"].as_str().unwrap().to_string();

    let (status, body) = post_json(
        &router,
        "/auth/two-factor/setup",
        Some("acme"),
        Some(&setup_bridge),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mandated setup failed: {body}");
    let emp_secret = body["secret"].as_str().unwrap().to_string();

    let code = totp::current_code(&emp_secret).unwrap();
    let (status, _) = post_json(
        &router,
        "/auth/two-factor/confirm",
        Some("acme"),
        Some(&setup_bridge),
        serde_json::json!({ "code": code }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // From now on the employee logs in with password + code.
    let (_, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        serde_json::json!({ "login": "emp", "password": "employeepass1" }),
    )
    .await;
    assert_eq!(body["status"], "two_factor_required");
    let emp_bridge = body["two_factor_token"].as_str().unwrap().to_string();
    let code = totp::current_code(&emp_secret).unwrap();
    let (status, body) = post_json(
        &router,
        "/auth/login/two-factor",
        Some("acme"),
        Some(&emp_bridge),
        serde_json::json!({ "code": code }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let emp_access = body["access_token"].as_str().unwrap().to_string();

    // Disabling 2FA is refused while the company mandates it.
    let (status, body) = post_json(
        &router,
        "/auth/two-factor/disable",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({ "password": "employeepass1" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");

    // Non-admins cannot change the company policy.
    let (status, _) = post_json(
        &router,
        "/auth/tenant/two-factor",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({ "required": false }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // -- The company sets a password policy.
    //
    // The employee has to sign in afresh after every password change (the
    // security stamp rotates), and 2FA is mandatory by now, so signing in
    // takes both steps.
    let sign_in = |password: &'static str, secret: String| {
        let router = router.clone();
        async move {
            let (_, body) = post_json(
                &router,
                "/auth/login",
                Some("acme"),
                None,
                serde_json::json!({ "login": "emp", "password": password }),
            )
            .await;
            assert_eq!(body["status"], "two_factor_required", "got: {body}");
            let bridge = body["two_factor_token"].as_str().unwrap().to_string();
            let code = totp::current_code(&secret).unwrap();
            post_json(
                &router,
                "/auth/login/two-factor",
                Some("acme"),
                Some(&bridge),
                serde_json::json!({ "code": code }),
            )
            .await
        }
    };

    let (status, body) = put_json(
        &router,
        "/auth/tenant/password-policy",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "min_length": 12,
            "require_digit": true,
            "history_count": 2,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "policy update failed: {body}");
    assert_eq!(body["policy"]["min_length"], 12);
    assert_eq!(body["policy"]["require_digit"], true);
    // Untouched rules still follow the deployment, and say so.
    assert_eq!(body["policy"]["require_symbol"], false);
    assert_eq!(body["overrides"]["require_symbol"], serde_json::Value::Null);
    assert_eq!(body["floor"]["min_length"], 8);

    // The deployment's own settings are a floor, not just a default: a
    // company cannot drop below what the deployment insists on.
    let (status, body) = put_json(
        &router,
        "/auth/tenant/password-policy",
        Some("acme"),
        Some(&access),
        serde_json::json!({ "min_length": 4 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");

    // Any user may read the policy — they meet it when changing password.
    let (status, body) = get_json(
        &router,
        "/auth/tenant/password-policy",
        Some("acme"),
        Some(&emp_access),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["policy"]["min_length"], 12);

    // ...but not write it.
    let (status, _) = put_json(
        &router,
        "/auth/tenant/password-policy",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({ "min_length": 20 }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A password that breaks the policy is refused, and the message names
    // every rule it broke rather than only the first.
    let (status, body) = post_json(
        &router,
        "/auth/password",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({ "current_password": "employeepass1", "new_password": "short" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");
    let detail = body["detail"].as_str().unwrap_or_default().to_string();
    assert!(detail.contains("12 characters"), "got: {detail}");
    assert!(detail.contains("digit"), "got: {detail}");

    // A compliant one is accepted.
    let (status, body) = post_json(
        &router,
        "/auth/password",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({
            "current_password": "employeepass1",
            "new_password": "employeepass2",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "password change failed: {body}");

    // -- Reuse: the retired password is refused while it is still in the
    // window, and so is the current one.
    let (status, body) = sign_in("employeepass2", emp_secret.clone()).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let emp_access = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = post_json(
        &router,
        "/auth/password",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({
            "current_password": "employeepass2",
            "new_password": "employeepass1",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "reuse must be refused: {body}");

    let (status, body) = post_json(
        &router,
        "/auth/password",
        Some("acme"),
        Some(&emp_access),
        serde_json::json!({
            "current_password": "employeepass2",
            "new_password": "employeepass2",
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "re-setting the current password must be refused: {body}"
    );

    // -- Expiry forces a change before any session is issued.
    let (status, body) = put_json(
        &router,
        "/auth/tenant/password-policy",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "min_length": 12,
            "require_digit": true,
            "history_count": 2,
            "expiry_days": 1,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");

    sea_orm::ConnectionTrait::execute_unprepared(
        &admin_db,
        "UPDATE users SET password_changed_at = now() - interval '10 days' \
         WHERE user_name = 'emp'",
    )
    .await
    .expect("backdating must work");

    // The password step alone does not reveal the expiry: the second
    // factor comes first, so a stolen password cannot reach the forced
    // change and take the account.
    let (_, body) = post_json(
        &router,
        "/auth/login",
        Some("acme"),
        None,
        serde_json::json!({ "login": "emp", "password": "employeepass2" }),
    )
    .await;
    assert_eq!(body["status"], "two_factor_required", "got: {body}");
    let bridge = body["two_factor_token"].as_str().unwrap().to_string();

    let code = totp::current_code(&emp_secret).unwrap();
    let (status, body) = post_json(
        &router,
        "/auth/login/two-factor",
        Some("acme"),
        Some(&bridge),
        serde_json::json!({ "code": code }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["status"], "password_expired", "got: {body}");
    let password_token = body["password_token"].as_str().unwrap().to_string();

    // The bridge is not a session.
    let (status, _) = get_json(&router, "/auth/me", Some("acme"), Some(&password_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The replacement is held to the policy like any other.
    let (status, _) = post_json(
        &router,
        "/auth/password/expired",
        Some("acme"),
        Some(&password_token),
        serde_json::json!({ "new_password": "nodigitshereok" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = post_json(
        &router,
        "/auth/password/expired",
        Some("acme"),
        Some(&password_token),
        serde_json::json!({ "new_password": "employeepass3" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "forced change failed: {body}");

    // And the new password signs in cleanly. The forced change rotated
    // the security stamp, so the employee's old token is dead and this is
    // the only live one.
    let (status, body) = sign_in("employeepass3", emp_secret.clone()).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["status"], "success", "got: {body}");
    let emp_access = body["access_token"].as_str().unwrap().to_string();

    // -- The company's mail server. Nothing is sent here: that needs a
    // real SMTP server. What is worth proving is that the password goes
    // in, never comes back, and is not sitting in the table in the clear.
    let (status, body) = get_json(&router, "/auth/tenant/mail", Some("acme"), Some(&access)).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["configured"], false);

    let (status, body) = put_json(
        &router,
        "/auth/tenant/mail",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "host": "smtp.acme.test",
            "port": 587,
            "username": "postmaster@acme.test",
            "password": "s3cr3t-smtp-password",
            "encryption": "starttls",
            "from_address": "billing@acme.test",
            "from_name": "Acme Billing",
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mail settings failed: {body}");
    assert_eq!(body["settings"]["password_set"], true);
    assert!(
        !body.to_string().contains("s3cr3t-smtp-password"),
        "the password must never come back: {body}"
    );

    // The column holds ciphertext, not the password.
    use sea_orm::ConnectionTrait;
    let stored: Option<String> = admin_db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT password_encrypted FROM tenant_mail_settings LIMIT 1",
        ))
        .await
        .expect("query must work")
        .expect("a row must exist")
        .try_get("", "password_encrypted")
        .expect("column must read");
    let stored = stored.expect("a password must be stored");
    assert!(
        !stored.contains("s3cr3t-smtp-password"),
        "the password must not be stored in the clear"
    );

    // Saving again without a password keeps the stored one, so a form
    // that never held the password can still round-trip.
    let (status, body) = put_json(
        &router,
        "/auth/tenant/mail",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "host": "smtp.acme.test",
            "port": 465,
            "username": "postmaster@acme.test",
            "encryption": "tls",
            "from_address": "billing@acme.test",
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["settings"]["password_set"], true, "got: {body}");
    assert_eq!(body["settings"]["port"], 465);

    // An empty password clears it.
    let (status, body) = put_json(
        &router,
        "/auth/tenant/mail",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "host": "smtp.acme.test",
            "port": 465,
            "username": "postmaster@acme.test",
            "password": "",
            "encryption": "tls",
            "from_address": "billing@acme.test",
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["settings"]["password_set"], false, "got: {body}");

    // A nonsense sender is refused rather than stored to fail later.
    let (status, _) = put_json(
        &router,
        "/auth/tenant/mail",
        Some("acme"),
        Some(&access),
        serde_json::json!({
            "host": "smtp.acme.test",
            "port": 465,
            "encryption": "tls",
            "from_address": "not-an-address",
            "enabled": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Mail settings describe the company's infrastructure; an ordinary
    // user has no business reading them.
    let (status, _) = get_json(
        &router,
        "/auth/tenant/mail",
        Some("acme"),
        Some(&emp_access),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn totp_codes_verify_and_recovery_codes_are_single_use() {
    let secret = totp::generate_secret();
    let code = totp::current_code(&secret).unwrap();
    assert!(totp::verify_code(&secret, &code).unwrap());
    assert!(!totp::verify_code(&secret, "000000").unwrap_or(true) || code == "000000");

    let codes = totp::generate_recovery_codes();
    let stored = totp::hash_recovery_codes(&codes);
    let remaining = totp::consume_recovery_code(&stored, &codes[3]).expect("code must match");
    assert!(totp::consume_recovery_code(&remaining, &codes[3]).is_none());
    assert!(totp::consume_recovery_code(&remaining, &codes[4]).is_some());
}
