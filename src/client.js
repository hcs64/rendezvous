'use strict';

let content = document.getElementById('content');
let submit = document.getElementById('submit-button');
let abort = document.getElementById('abort-button');
let log = document.getElementById('log');
let statusdiv = document.getElementById('status-div');
let link = document.getElementById('link');
let uploadmeter = document.getElementById('uploadmeter');

let reportStatus = function (newStatus) {
  log.value += newStatus + '\n';
  log.scrollTop = log.scrollHeight;
};

submit.disabled = false;
log.value = '';

function addEventHandlers(xhr, loadedCallback, errorCallback) {
  xhr.onload = function () {
    if (xhr.status != 200) {
      errorCallback('http-error', xhr);
    } else {
      loadedCallback(xhr);
    }
  };
  xhr.onerror = function () {
    errorCallback('error', xhr);
  };
  xhr.onabort = function () {
    errorCallback('abort', xhr);
  };
}

function requestToken(loadedCallback, errorCallback) {
  reportStatus('Requesting upload token');

  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/token/request', true);
  addEventHandlers(xhr, loadedCallback, errorCallback);
  xhr.send();
}

function cancelToken(token, secret) {
  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/token/cancel?token=' + token + '&secret=' + secret);
  xhr.send();
}

function upload(token, secret, loadedCallback, errorCallback) {
  reportStatus('Starting upload');

  let data = content.value;
  let xhr = new XMLHttpRequest();
  xhr.open('POST', '/upload?token=' + token + '&secret=' + secret);
  xhr.setRequestHeader("Content-Type", "text/plain");
  addEventHandlers(xhr, loadedCallback, errorCallback);
  xhr.send(content.value);
}

submit.onclick = function onclickSubmit () {
  let token = '';
  let secret = '';
  let uploads = 0;
  let errors = 0;
  link.value = '';
  uploadmeter.innerText = '0';

  submit.hidden = true;
  //abort.hidden = false;

  statusdiv.hidden = false;
  log.value = '';

  submit.disabled = true;

  function uploadSuccess (xhr) {
    reportStatus('Upload ok: "' + xhr.responseText + '"')
    uploads += 1;
    uploadmeter.innerText = '' + uploads;
    errors = 0;

    // get it ready to go again
    upload(token, secret, uploadSuccess, uploadError);
  }

  function uploadError (type, xhr) {
    if (type === 'error') {
      reportStatus('Upload failed.');
    } else if (type === 'http-error') {
      reportStatus('Upload failed with HTTP error ' + xhr.status + ', ' + xhr.responseText);
    } else if (type === 'abort') {
      reportStatus('Upload aborted.');
    }

    reportStatus('Upload failed: "' + xhr.responseText + '"')

    errors += 1;

    if (errors < 20) {
      // try to start a new upload anyway
      // TODO check what the error actually was
      upload(token, secret, uploadSuccess, uploadError);
    } else {
      reportStatus('Too many errors, giving up');
      submit.hidden = false;
    }
  }

  requestToken(
    function requestTokenLoaded (xhr) {
      let parts = xhr.responseText.split(',');
      token = parts[0];
      secret = parts[1];
      uploads = 0;
      uploadmeter.innerText = '0';

      let url = window.location;
      link.value = url.protocol + '//' + url.host + '/download?' + token;
      link.size = '' + (link.value.length);

      reportStatus('Got token');
      upload(token, secret, uploadSuccess, uploadError);
    },
    function requestTokenError (type, xhr) {
      // TODO retry?
      reportStatus('Request failed.');
    }
  );
};
